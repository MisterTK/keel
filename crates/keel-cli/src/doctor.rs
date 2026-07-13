//! `keel doctor` — the honesty report (dx-spec §2).
//!
//! Three questions, answered from files (no program run):
//! 1. **Coverage.** What's *wrapped* (observed in `.keel/discovery.db`), what's
//!    *visible-but-unwrapped* (found by the static scan, never seen at runtime),
//!    and what's *invisible* (an effect library with no adapter — Keel can't
//!    wrap what it can't see).
//! 2. **Adapters.** A registry of the known adapter set, each pinned (contract-
//!    tested against a version) or best-effort, annotated with what was detected.
//! 3. **Policy.** `keel.toml` validated against the typed model
//!    ([`keel_core_api::policy::Policy`]); on error, the exact field path.
//! 4. **Journal.** Where the journal lives, resolved the way the engine
//!    resolves it at configure time (`journal` key, else `.keel/journal.db`).
//!    A location this build has no backend for (`postgres://`) is an error
//!    finding: the app will fail to configure with KEEL-E005.
//!
//! Every finding carries a suggested action, and the whole thing has a `--json`
//! twin. An invalid policy — or a journal backend this build cannot provide —
//! exits [`EXIT_USAGE`](crate::EXIT_USAGE); otherwise 0.

use std::collections::BTreeSet;
use std::path::Path;

use keel_core_api::policy::Policy;
use serde::Serialize;

use crate::diff::{PolicyOp, PolicyPath, Proposal, propose, resolve_dotted_path};
use crate::render::to_json;
use crate::scan::ScanResult;
use crate::{EXIT_OK, EXIT_USAGE, Rendered, evidence, scan};

/// One known adapter/pack: its library, the language(s), the semantic target
/// class it exposes, and whether it is version-pinned or best-effort.
#[derive(Debug, Clone, Copy, Serialize)]
struct Adapter {
    best_effort: bool,
    lang: &'static str,
    lib: &'static str,
    target: &'static str,
}

/// The compiled adapter registry (dx-spec §2/§4). "data compiled from the known
/// adapter set"; the front ends register these at import time, but the CLI knows
/// the set statically so `doctor` works without running the program.
const REGISTRY: &[Adapter] = &[
    Adapter {
        lib: "httpx",
        lang: "python",
        target: "host",
        best_effort: false,
    },
    Adapter {
        lib: "requests",
        lang: "python",
        target: "host",
        best_effort: false,
    },
    Adapter {
        lib: "aiohttp",
        lang: "python",
        target: "host",
        best_effort: true,
    },
    Adapter {
        lib: "urllib3",
        lang: "python",
        target: "host",
        best_effort: true,
    },
    Adapter {
        lib: "boto3",
        lang: "python",
        target: "tool:aws.*",
        best_effort: true,
    },
    Adapter {
        lib: "psycopg",
        lang: "python",
        target: "host",
        best_effort: true,
    },
    Adapter {
        lib: "openai",
        lang: "python+node",
        target: "llm:openai",
        best_effort: false,
    },
    Adapter {
        lib: "anthropic",
        lang: "python+node",
        target: "llm:anthropic",
        best_effort: false,
    },
    Adapter {
        lib: "fetch",
        lang: "node",
        target: "host",
        best_effort: false,
    },
    Adapter {
        lib: "undici",
        lang: "node",
        target: "host",
        best_effort: true,
    },
    Adapter {
        lib: "http",
        lang: "node",
        target: "host",
        best_effort: true,
    },
    Adapter {
        lib: "ai-sdk",
        lang: "node",
        target: "llm:*",
        best_effort: false,
    },
    Adapter {
        lib: "mcp",
        lang: "node",
        target: "mcp:*",
        best_effort: true,
    },
];

/// One line in the adapter section: a registry entry plus whether this project
/// uses it.
#[derive(Debug, Serialize)]
struct AdapterStatus {
    detected: bool,
    lib: &'static str,
    status: &'static str,
    target: &'static str,
}

/// The three coverage classes.
#[derive(Debug, Serialize)]
struct Coverage {
    invisible: Vec<String>,
    visible_unwrapped: Vec<String>,
    wrapped: Vec<String>,
}

/// A policy-validation outcome.
#[derive(Debug, Serialize)]
struct PolicyCheck {
    field: Option<String>,
    message: Option<String>,
    present: bool,
    valid: bool,
}

/// One actionable finding. Where the finding implies a policy edit, `fix`
/// carries the applyable form (dx-spec §5, diffs as the lingua franca): a
/// unified `patch` for `git apply` plus structured `changes`.
#[derive(Debug, Serialize)]
struct Finding {
    action: String,
    detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    fix: Option<Proposal>,
    level: &'static str,
    topic: &'static str,
}

/// Where the journal lives, as resolved for this project — the same selection
/// the engine makes at configure time.
#[derive(Debug, Serialize)]
struct JournalReport {
    /// `"sqlite"` (default and `file:` locations) or `"postgres"`.
    backend: &'static str,
    /// The location as users should read it: a `file:` path as written, the
    /// default relative path, or a credential-redacted `postgres://` form.
    location: String,
    /// `"keel.toml"` when the `journal` key set it, else `"default"`.
    source: &'static str,
    /// `false` when this build has no backend for the location — the app will
    /// fail to configure with KEEL-E005.
    supported: bool,
}

impl JournalReport {
    fn from_resolved(resolved: &evidence::ResolvedJournal) -> Self {
        Self {
            backend: resolved.backend.as_str(),
            location: resolved.display.clone(),
            source: if resolved.from_policy {
                "keel.toml"
            } else {
                "default"
            },
            supported: resolved.backend == evidence::JournalBackendKind::Sqlite,
        }
    }
}

/// The whole doctor report.
#[derive(Debug, Serialize)]
struct DoctorReport {
    adapters: Vec<AdapterStatus>,
    coverage: Coverage,
    findings: Vec<Finding>,
    journal: JournalReport,
    ok: bool,
    policy: PolicyCheck,
}

/// A policy validation outcome plus, when it failed on a specific field, the
/// applyable fix: remove the offending entry. Keel's documented semantics make
/// removal always safe — "delete anything; defaults still apply" — so the
/// suggested patch drops the invalid entry rather than guessing a value.
#[derive(Debug)]
struct PolicyValidation {
    check: PolicyCheck,
    fix: Option<Proposal>,
}

/// Run `keel doctor` for `project`.
pub fn run(project: &Path) -> Rendered {
    let scan = scan::scan(project);
    let discovery = match evidence::read_discovery(project) {
        Ok(d) => d.into_iter().map(|s| s.target).collect(),
        Err(e) => {
            return Rendered {
                human: format!("keel \u{25b8} doctor unavailable: {e}"),
                json: to_json(&serde_json::json!({ "error": e })),
                exit: crate::EXIT_FAILURE,
                to_stderr: true,
            };
        }
    };
    let policy = validate_policy(&evidence::keel_toml(project));
    let journal = JournalReport::from_resolved(&evidence::resolved_journal(project));
    let report = build_report(&scan, &discovery, policy, journal);
    let exit = if report.ok { EXIT_OK } else { EXIT_USAGE };
    let human = human(&report);
    Rendered::ok(human, to_json(&report)).with_exit(exit)
}

/// An unsupported journal backend is an error finding: the app would fail to
/// configure with KEEL-E005, so doctor must not read clean.
fn journal_finding(journal: &JournalReport) -> Option<Finding> {
    (!journal.supported).then(|| Finding {
        action: "Use a `file:` location (or drop the key for the default .keel/journal.db); Postgres support is future work — see docs.".to_owned(),
        detail: format!(
            "keel.toml sets `journal` to a {} location, but this build has no {} backend — the app will fail to configure with KEEL-E005.",
            journal.backend, journal.backend
        ),
        fix: None,
        level: "error",
        topic: "journal",
    })
}

/// Assemble the report from the four evidence inputs. Pure, so the golden test
/// pins it without a filesystem or `python3`.
fn build_report(
    scan: &ScanResult,
    wrapped_targets: &BTreeSet<String>,
    policy: PolicyValidation,
    journal: JournalReport,
) -> DoctorReport {
    let PolicyValidation { check: policy, fix } = policy;
    let registry_libs: BTreeSet<&str> = REGISTRY.iter().map(|a| a.lib).collect();

    // Coverage from the target sets.
    let visible: BTreeSet<&String> = scan.targets.keys().collect();
    let wrapped: Vec<String> = wrapped_targets.iter().cloned().collect();
    let visible_unwrapped: Vec<String> = visible
        .iter()
        .filter(|t| !wrapped_targets.contains(**t))
        .map(|t| (*t).clone())
        .collect();
    let invisible: Vec<String> = scan
        .libs
        .iter()
        .filter(|lib| !registry_libs.contains(lib.as_str()))
        .cloned()
        .collect();

    // Adapter registry annotated with detection.
    let adapters: Vec<AdapterStatus> = REGISTRY
        .iter()
        .map(|a| AdapterStatus {
            detected: scan.libs.contains(a.lib),
            lib: a.lib,
            status: if a.best_effort {
                "best-effort"
            } else {
                "pinned"
            },
            target: a.target,
        })
        .collect();

    // Findings + suggested actions.
    let mut findings = Vec::new();
    for target in &visible_unwrapped {
        findings.push(Finding {
            action:
                "Run `keel run <script>` so Keel can confirm this target is wrapped at runtime."
                    .to_owned(),
            detail: format!(
                "`{target}` is visible in your code but has no observed runtime evidence."
            ),
            fix: None,
            level: "warn",
            topic: "visible-unwrapped",
        });
    }
    for lib in &invisible {
        findings.push(Finding {
            action: format!("No adapter for `{lib}` yet — its calls are invisible to Keel. Track adapter support or wrap manually."),
            detail: format!("`{lib}` is imported but has no adapter in the registry."),
            fix: None,
            level: "warn",
            topic: "invisible",
        });
    }
    // Always: the honest advisory about what static + adapter interception can't see.
    findings.push(Finding {
        action: "If a dependency makes calls Keel never reports, file an adapter request.".to_owned(),
        detail: "Raw sockets and unknown native libraries are invisible to static and adapter-based interception.".to_owned(),
        fix: None,
        level: "info",
        topic: "invisible",
    });
    if !policy.valid && policy.present {
        let field = policy.field.clone().unwrap_or_default();
        let mut action = "Fix the field above, then re-run `keel doctor`; validate against contracts/policy.schema.json.".to_owned();
        if fix.is_some() {
            action.push_str(
                " Or apply the attached patch (`git apply`) to remove the invalid entry — defaults cover it.",
            );
        }
        findings.push(Finding {
            action,
            detail: format!(
                "keel.toml failed validation at `{field}`: {}",
                policy.message.clone().unwrap_or_default()
            ),
            fix,
            level: "error",
            topic: "policy",
        });
    }
    findings.extend(journal_finding(&journal));

    let ok = (policy.valid || !policy.present) && journal.supported;
    DoctorReport {
        adapters,
        coverage: Coverage {
            invisible,
            visible_unwrapped,
            wrapped,
        },
        findings,
        journal,
        ok,
        policy,
    }
}

/// Validate `keel.toml` against the typed [`Policy`] model, reporting the exact
/// field path on error (via `serde_path_to_error`) and, when a field is at
/// fault, attaching the applyable removal fix.
fn validate_policy(path: &Path) -> PolicyValidation {
    if !path.exists() {
        return PolicyValidation {
            check: PolicyCheck {
                field: None,
                message: None,
                present: false,
                valid: true,
            },
            fix: None,
        };
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return invalid(None, "keel.toml exists but could not be read", None);
    };
    let toml_value: toml::Value = match text.parse() {
        Ok(v) => v,
        Err(e) => return invalid(None, &format!("keel.toml is not valid TOML: {e}"), None),
    };
    let json_value = match serde_json::to_value(&toml_value) {
        Ok(v) => v,
        Err(e) => {
            return invalid(
                None,
                &format!("keel.toml could not be normalized: {e}"),
                None,
            );
        }
    };
    match serde_path_to_error::deserialize::<_, Policy>(&json_value) {
        Ok(_) => PolicyValidation {
            check: PolicyCheck {
                field: None,
                message: None,
                present: true,
                valid: true,
            },
            fix: None,
        },
        Err(e) => {
            let field = e.path().to_string();
            let fix = suggest_removal(&text, &field);
            invalid(Some(field), &e.inner().to_string(), fix)
        }
    }
}

fn invalid(field: Option<String>, message: &str, fix: Option<Proposal>) -> PolicyValidation {
    PolicyValidation {
        check: PolicyCheck {
            field,
            message: Some(message.to_owned()),
            present: true,
            valid: false,
        },
        fix,
    }
}

/// The deepest path a removal fix targets: `target."…".<key>` — dropping the
/// whole top-level entry under the target keeps the remainder trivially valid,
/// where surgically deleting one nested field might leave an invalid stub.
const MAX_FIX_DEPTH: usize = 3;

/// Synthesize the applyable fix for an invalid policy field: delete the
/// offending entry (truncated to its top-level key under the target). Returns
/// `None` when the field path cannot be resolved back into the document.
fn suggest_removal(text: &str, field: &str) -> Option<Proposal> {
    let resolved = resolve_dotted_path(text, field)?;
    let segments = resolved.segments();
    let cut = segments.len().min(MAX_FIX_DEPTH);
    let path = PolicyPath::new(segments[..cut].iter().cloned());
    let proposal = propose(Some(text), &[PolicyOp::Remove { path }]).ok()?;
    if proposal.patch.is_empty() {
        None
    } else {
        Some(proposal)
    }
}

/// The human report, derived from [`DoctorReport`] so no fact escapes the JSON.
fn human(r: &DoctorReport) -> String {
    let mut out = String::from("keel \u{25b8} doctor\n");

    out.push_str("\ncoverage\n");
    line_list(&mut out, "  wrapped:          ", &r.coverage.wrapped);
    line_list(
        &mut out,
        "  visible-unwrapped:",
        &r.coverage.visible_unwrapped,
    );
    line_list(&mut out, "  invisible:        ", &r.coverage.invisible);

    out.push_str("\nadapters\n");
    for a in &r.adapters {
        let mark = if a.detected { "\u{2713}" } else { " " };
        let line = format!(
            "  [{mark}] {lib:<10} {status:<12} -> {target}\n",
            lib = a.lib,
            status = a.status,
            target = a.target,
        );
        out.push_str(&line);
    }

    out.push_str("\npolicy\n");
    if !r.policy.present {
        out.push_str("  no keel.toml — smart defaults apply. `keel init` to customize.\n");
    } else if r.policy.valid {
        out.push_str("  keel.toml is valid.\n");
    } else {
        let line = format!(
            "  keel.toml INVALID at `{}`: {}\n",
            r.policy.field.clone().unwrap_or_default(),
            r.policy.message.clone().unwrap_or_default(),
        );
        out.push_str(&line);
    }

    out.push_str("\njournal\n");
    let journal_line = if r.journal.supported {
        format!(
            "  {} at {} ({})\n",
            r.journal.backend, r.journal.location, r.journal.source
        )
    } else {
        format!(
            "  {} at {} ({}) — NOT supported in this build (KEEL-E005)\n",
            r.journal.backend, r.journal.location, r.journal.source
        )
    };
    out.push_str(&journal_line);

    if !r.findings.is_empty() {
        out.push_str("\nfindings\n");
        for f in &r.findings {
            let line = format!(
                "  [{}] {}\n        \u{2192} {}\n",
                f.level, f.detail, f.action
            );
            out.push_str(&line);
            if let Some(fix) = &f.fix {
                // Verbatim (unindented) so copy-paste into `git apply` works.
                out.push_str("        patch (apply with `git apply`):\n");
                out.push_str(&fix.patch);
            }
        }
    }
    let tail = format!(
        "\n{}\n",
        if r.ok {
            "ok"
        } else {
            "configuration error (exit 2)"
        }
    );
    out.push_str(&tail);
    out
}

fn line_list(out: &mut String, label: &str, items: &[String]) {
    let line = if items.is_empty() {
        format!("{label} (none)\n")
    } else {
        format!("{label} {}\n", items.join(", "))
    };
    out.push_str(&line);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{Sighting, TargetClass, TargetEvidence};

    /// The default journal report (no `journal` key in keel.toml).
    fn default_journal() -> JournalReport {
        JournalReport {
            backend: "sqlite",
            location: ".keel/journal.db".to_owned(),
            source: "default",
            supported: true,
        }
    }

    fn scan_with(target: &str, class: TargetClass, libs: &[&str]) -> ScanResult {
        let mut s = ScanResult {
            files_scanned: 1,
            python_available: true,
            ..ScanResult::default()
        };
        s.targets.insert(
            target.to_owned(),
            TargetEvidence {
                class,
                sightings: [Sighting {
                    file: "app.py".into(),
                    line: 1,
                }]
                .into_iter()
                .collect(),
            },
        );
        s.libs = libs.iter().map(|l| (*l).to_owned()).collect();
        s
    }

    #[test]
    fn wrapped_visible_and_invisible_are_classified() {
        // "django" stands in for any effect library with no adapter in the
        // registry (boto3/psycopg both gained one — see REGISTRY above).
        let scan = scan_with("llm:openai", TargetClass::Llm, &["openai", "django"]);
        // discovery observed a DIFFERENT target than the visible one.
        let wrapped: BTreeSet<String> = ["api.observed.com".to_owned()].into_iter().collect();
        let policy = PolicyValidation {
            check: PolicyCheck {
                field: None,
                message: None,
                present: false,
                valid: true,
            },
            fix: None,
        };
        let r = build_report(&scan, &wrapped, policy, default_journal());

        assert_eq!(r.coverage.wrapped, vec!["api.observed.com"]);
        assert_eq!(r.coverage.visible_unwrapped, vec!["llm:openai"]);
        assert_eq!(
            r.coverage.invisible,
            vec!["django"],
            "django has no adapter"
        );
        assert!(r.ok, "no policy present → ok");
        // openai adapter detected + pinned.
        let openai = r.adapters.iter().find(|a| a.lib == "openai").unwrap();
        assert!(openai.detected);
        assert_eq!(openai.status, "pinned");
    }

    #[test]
    fn invalid_policy_is_a_finding_and_not_ok() {
        let scan = ScanResult::default();
        let wrapped = BTreeSet::new();
        let policy = PolicyValidation {
            check: PolicyCheck {
                field: Some("target.x.retry.attempts".to_owned()),
                message: Some("invalid value: integer `0`".to_owned()),
                present: true,
                valid: false,
            },
            fix: None,
        };
        let r = build_report(&scan, &wrapped, policy, default_journal());
        assert!(!r.ok);
        assert!(
            r.findings
                .iter()
                .any(|f| f.topic == "policy" && f.level == "error")
        );
    }

    /// A `postgres://` journal has no backend in this build: doctor reports it,
    /// raises an error finding naming KEEL-E005, and exits non-ok — the app
    /// would fail to configure, so CI must not pass silently.
    #[test]
    fn unsupported_journal_backend_is_an_error_finding_and_not_ok() {
        let scan = ScanResult::default();
        let wrapped = BTreeSet::new();
        let policy = PolicyValidation {
            check: PolicyCheck {
                field: None,
                message: None,
                present: true,
                valid: true,
            },
            fix: None,
        };
        let journal = JournalReport {
            backend: "postgres",
            location: "postgres://\u{2026}@db.internal/keel".to_owned(),
            source: "keel.toml",
            supported: false,
        };
        let r = build_report(&scan, &wrapped, policy, journal);
        assert!(!r.ok, "an unbootable configuration must not be ok");
        let finding = r
            .findings
            .iter()
            .find(|f| f.topic == "journal")
            .expect("journal finding present");
        assert_eq!(finding.level, "error");
        assert!(finding.detail.contains("KEEL-E005"));
        assert!(finding.action.contains("file:"));
        // Human output carries the journal facts.
        let text = human(&r);
        assert!(text.contains("postgres"));
        assert!(text.contains("NOT supported"));
    }

    /// End-to-end over a real project dir: doctor resolves and reports the
    /// `file:` journal location from keel.toml.
    #[test]
    fn doctor_reports_the_policy_selected_journal_location() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("keel.toml"),
            "journal = \"file:custom/j.db\"\n",
        )
        .unwrap();
        let r = run(dir.path());
        assert_eq!(r.exit, EXIT_OK);
        assert_eq!(r.json["journal"]["backend"], "sqlite");
        assert_eq!(r.json["journal"]["location"], "custom/j.db");
        assert_eq!(r.json["journal"]["source"], "keel.toml");
        assert_eq!(r.json["journal"]["supported"], true);
        assert!(r.human.contains("custom/j.db"));
    }

    /// End-to-end: a `postgres://` journal exits `EXIT_USAGE`, with credentials
    /// redacted from both output forms.
    #[test]
    fn doctor_flags_a_postgres_journal_and_redacts_credentials() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("keel.toml"),
            "journal = \"postgres://keel:sekrit@db.internal/keel\"\n",
        )
        .unwrap();
        let r = run(dir.path());
        assert_eq!(r.exit, EXIT_USAGE);
        assert_eq!(r.json["journal"]["backend"], "postgres");
        assert_eq!(r.json["journal"]["supported"], false);
        assert_eq!(r.json["ok"], false);
        let json_text = crate::render::json_string(&r.json);
        assert!(!json_text.contains("sekrit"), "credentials never printed");
        assert!(!r.human.contains("sekrit"), "credentials never printed");
    }

    #[test]
    fn validate_policy_reports_exact_field_path() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("keel.toml");
        std::fs::write(&path, "[target.\"x\"]\nretry = { attempts = 0 }\n").unwrap();
        let v = validate_policy(&path);
        assert!(!v.check.valid);
        assert_eq!(v.check.field.as_deref(), Some("target.x.retry.attempts"));
    }

    #[test]
    fn validate_policy_accepts_a_good_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("keel.toml");
        std::fs::write(
            &path,
            "[target.\"api.x\"]\nretry = { attempts = 5, schedule = \"exp(200ms, x2, max 30s, jitter)\" }\n",
        )
        .unwrap();
        let v = validate_policy(&path);
        assert!(
            v.check.valid,
            "field={:?} msg={:?}",
            v.check.field, v.check.message
        );
        assert!(v.fix.is_none(), "a valid policy needs no fix");
    }

    #[test]
    fn absent_policy_is_valid_and_ok() {
        let v = validate_policy(Path::new("/nonexistent/keel.toml"));
        assert!(v.check.valid);
        assert!(!v.check.present);
    }

    /// dx-spec §5: the invalid-policy finding carries an *applyable* fix — a
    /// patch that removes the offending entry (defaults cover it) while every
    /// untouched byte, comments included, survives.
    #[test]
    fn invalid_policy_finding_carries_an_applyable_removal_fix() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("keel.toml");
        std::fs::write(
            &path,
            "# my tuning\n[target.\"api.example.com\"]\ntimeout = \"30s\" # keep\nretry = { attempts = 0 }\n",
        )
        .unwrap();

        let v = validate_policy(&path);
        assert!(!v.check.valid);
        assert_eq!(
            v.check.field.as_deref(),
            Some("target.api.example.com.retry.attempts"),
            "dotted host key resolves"
        );
        let fix = v.fix.expect("fix proposal attached");
        assert!(fix.patch.starts_with("--- a/keel.toml\n+++ b/keel.toml\n"));
        // The patch is faithful: applying it reproduces the proposed text.
        let applied =
            crate::diff::apply_unified(&std::fs::read_to_string(&path).unwrap(), &fix.patch)
                .unwrap();
        assert_eq!(applied, fix.new_text);
        // The proposed text is a valid policy with the untouched bytes intact.
        std::fs::write(&path, &fix.new_text).unwrap();
        let after = validate_policy(&path);
        assert!(after.check.valid, "removal fix yields a valid policy");
        assert!(fix.new_text.contains("# my tuning"));
        assert!(fix.new_text.contains("timeout = \"30s\" # keep"));
        assert!(
            !fix.new_text.contains("retry"),
            "whole invalid entry removed"
        );
        // The structured form names the removed entry.
        assert_eq!(fix.changes.len(), 1);
        assert_eq!(fix.changes[0].path, "target.\"api.example.com\".retry");
        assert!(fix.changes[0].after.is_none());
    }

    /// A file that is not even TOML has no field to fix — no patch is attached.
    #[test]
    fn unparseable_policy_has_no_fix() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("keel.toml");
        std::fs::write(&path, "not [valid toml\n").unwrap();
        let v = validate_policy(&path);
        assert!(!v.check.valid);
        assert!(v.fix.is_none());
    }
}
