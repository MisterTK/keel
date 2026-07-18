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
use crate::scan::{ScanResult, TransportClass};
use crate::{EXIT_OK, EXIT_USAGE, Rendered, agents_cli, evidence, scan};

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
    // The six agent-framework packs (dx-spec agent-first-class work) plus the
    // google-genai LLM provider pack. Farm-certification alone isn't the
    // `best_effort` discriminator — aiohttp/boto3 below are farm-tested too,
    // yet stay best-effort because each carries a documented fidelity gap
    // (aiohttp's cache-hit replay is a duck-typed stand-in response, not a
    // real `aiohttp.ClientResponse`; boto3 infers retry-safety from an
    // operation-name-prefix heuristic, not a guaranteed contract). These
    // packs instead wrap official, stable extension points with no such gap
    // (ADK's plugin API, AI-SDK-style documented seams) and are ALSO
    // farm-certified (.github/workflows/adapter-farm.yml) — so pinned like
    // httpx/openai. `target` mirrors each pack's own declared
    // `TargetDecl.pattern` exactly (adk_pack.py, pydantic_ai_pack.py,
    // openai_agents_pack.py, crewai_pack.py, langgraph_pack.py:
    // `"tool:<name>"`; mcp_pack.py / mcp.mjs: `"mcp:<server>"`).
    Adapter {
        lib: "google-adk",
        lang: "python",
        target: "tool:<name>",
        best_effort: false,
    },
    Adapter {
        lib: "google-genai",
        lang: "python",
        target: "llm:google-genai",
        best_effort: false,
    },
    Adapter {
        lib: "pydantic-ai",
        lang: "python",
        target: "tool:<name>",
        best_effort: false,
    },
    Adapter {
        lib: "openai-agents",
        lang: "python",
        target: "tool:<name>",
        best_effort: false,
    },
    Adapter {
        lib: "crewai",
        lang: "python",
        target: "tool:<name>",
        best_effort: false,
    },
    Adapter {
        lib: "langgraph",
        lang: "python",
        target: "tool:<name>",
        best_effort: false,
    },
    // The `mcp` client SDK — one row shared by Python (mcp_pack) and Node
    // (mcp.mjs), like the openai/anthropic rows above: same import/package
    // name in both runtimes, and both packs declare the identical
    // per-server target grammar (`TargetDecl.pattern == "mcp:<server>"` in
    // both mcp_pack.py and mcp.mjs). Farm-certified in both languages
    // (tests/test_farm_mcp.py, node/keel/test/mcp-farm.test.mjs).
    Adapter {
        lib: "mcp",
        lang: "python+node",
        target: "mcp:<server>",
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
        lib: "ioredis",
        lang: "node",
        target: "host",
        best_effort: true,
    },
    Adapter {
        lib: "mysql2",
        lang: "node",
        target: "host",
        best_effort: true,
    },
    Adapter {
        lib: "pg",
        lang: "node",
        target: "host",
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

/// One host `keel doctor` judged excluded or unreachable, with the honest
/// reason why — see [`Topology`]. `pub(crate)`: `init.rs` reuses
/// [`classify_topology`] to skip proposals for excluded hosts and print why.
#[derive(Debug, Serialize)]
pub(crate) struct TopologyEntry {
    pub(crate) host: String,
    pub(crate) reason: String,
}

/// One externally-launched process the scan saw — traffic inside it is
/// outside Keel's visibility regardless of policy (dx-spec's "shouldn't/
/// can't/wrap it" honesty triad, the "external process" leg). `pub(crate)`
/// alongside [`Topology`] for the same cross-module reuse.
#[derive(Debug, Serialize)]
pub(crate) struct ExternalProcess {
    pub(crate) command: String,
    pub(crate) file: String,
    pub(crate) launcher: String,
    pub(crate) line: u32,
}

/// The three-bucket honesty topology (dx-spec §2): every host Keel's static
/// scan saw, sorted into exactly one of "wrap it" (a tracked transport is in
/// reach, or the target is wrapped-at-runtime/`llm:*` by construction),
/// "can't reach it" (no adapted transport in reach — Keel is blind here
/// regardless of policy), or "shouldn't reach it" (sighted only inside a
/// file the scan judged dependency-averse — excluded from proposed policy on
/// purpose). `external_processes` is the adjacent, host-independent honesty
/// signal: traffic inside an externally-launched process Keel cannot see at
/// all, no matter which bucket its host would otherwise land in.
///
/// `pub(crate)`: `init.rs` reuses [`classify_topology`] to skip proposing
/// policy for excluded hosts and print why.
#[derive(Debug, Serialize)]
pub(crate) struct Topology {
    pub(crate) excluded: Vec<TopologyEntry>,
    pub(crate) external_processes: Vec<ExternalProcess>,
    pub(crate) unreachable: Vec<TopologyEntry>,
    pub(crate) wrappable: Vec<String>,
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
    topology: Topology,
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

/// A one-line advisory for `keel run`'s pre-exec preflight step (dx-spec's
/// "before any calls fire" — a static scan of the whole source tree, run by
/// the Rust CLI before the target process even starts, sees this more
/// faithfully than hooking the Python/Node in-process bootstrap could: that
/// bootstrap runs before the target script's own imports execute, so it
/// could not actually see them yet). `None` when there's nothing to warn
/// about — never runs `keel doctor`'s full report, just the one check that's
/// cheap and relevant at this point.
#[must_use]
pub fn preflight_advisory(project: &Path) -> Option<String> {
    let scan = scan::scan(project);
    let registry_libs: BTreeSet<&str> = REGISTRY.iter().map(|a| a.lib).collect();
    let finding = resilience_finding(&scan, &registry_libs)?;
    Some(format!(
        "keel \u{25b8} {}\n  next: {} (run `keel doctor --json` for detail; skip this check with \
         --no-preflight or KEEL_SKIP_PREFLIGHT=1)",
        finding.detail, finding.action
    ))
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
    let agents_cli_finding = agents_cli_placement_finding(project);
    let report = build_report(&scan, &discovery, policy, journal, agents_cli_finding);
    let exit = if report.ok { EXIT_OK } else { EXIT_USAGE };
    let human = human(&report);
    Rendered::ok(human, to_json(&report)).with_exit(exit)
}

/// A pre-existing resilience library (e.g. `tenacity`, `backoff`) risks
/// silently compounding with Keel's own retry/backoff/breaker — Keel patches
/// at the transport layer, below this kind of user code, so it has no
/// visibility into whether the library is actually configured to retry the
/// same calls Keel wraps. Emitted only when the project ALSO uses at least
/// one adapter library Keel wraps (`registry_libs`): a resilience library
/// imported for something unrelated to any Keel-wrapped effect is not
/// evidence of compounding, and flagging it anyway would be a false
/// positive that erodes trust in doctor's other findings. Python-only as of
/// this build (`scan::python`'s `RESILIENCE_LIBS`) — Node has no equivalent
/// signal today (Keel doesn't even adapt `axios`, the library most
/// associated with `axios-retry`), documented as accepted debt rather than
/// silently absent.
fn resilience_finding(scan: &ScanResult, registry_libs: &BTreeSet<&str>) -> Option<Finding> {
    let compounds_with = scan
        .libs
        .iter()
        .any(|lib| registry_libs.contains(lib.as_str()));
    if scan.resilience_libs.is_empty() || !compounds_with {
        return None;
    }
    let libs: Vec<&str> = scan.resilience_libs.iter().map(String::as_str).collect();
    Some(Finding {
        action: "Delete the old retry/backoff code if Keel's policy now covers it, or scope \
                 Keel's policy to skip this target (e.g. `attempts = 1`) if you want to keep \
                 relying on it — don't leave both running unconfigured against each other."
            .to_owned(),
        detail: format!(
            "This project imports {} alongside at least one library Keel wraps — Keel cannot \
             see whether {} is actually configured to retry the same calls, so retries may be \
             silently compounding.",
            libs.join(", "),
            if libs.len() == 1 { "it" } else { "they" }
        ),
        fix: None,
        level: "warn",
        topic: "preexisting-resilience",
    })
}

/// The three honesty findings that carry [`Topology`]'s buckets into the
/// findings list: one `url-no-transport` warning per unreachable host, one
/// `subprocess-blind-spot` warning naming every externally-launched process
/// (if any), and one `dependency-averse-excluded` info per excluded host.
/// None of these are configuration errors — they never affect `ok`.
fn topology_findings(topology: &Topology) -> Vec<Finding> {
    let mut findings = Vec::new();
    for entry in &topology.unreachable {
        findings.push(Finding {
            action: "Trace how this request is actually dispatched before proposing policy; if \
                      it is stdlib urllib, adapter support is tracked."
                .to_owned(),
            detail: format!("`{}` — {}.", entry.host, entry.reason),
            fix: None,
            level: "warn",
            topic: "url-no-transport",
        });
    }
    if !topology.external_processes.is_empty() {
        let cmds: Vec<String> = topology
            .external_processes
            .iter()
            .map(|p| format!("`{}` ({} at {}:{})", p.command, p.launcher, p.file, p.line))
            .collect();
        findings.push(Finding {
            action: "Confirm none of these processes carry traffic you care about; Keel must be \
                      installed inside a process to see it."
                .to_owned(),
            detail: format!(
                "Keel cannot see traffic inside {} externally-launched process(es): {}.",
                topology.external_processes.len(),
                cmds.join(", ")
            ),
            fix: None,
            level: "warn",
            topic: "subprocess-blind-spot",
        });
    }
    for entry in &topology.excluded {
        findings.push(Finding {
            action: "Confirm the exclusion is intended; add `# keel: include` to the file to \
                      override."
                .to_owned(),
            detail: format!("`{}` — {}.", entry.host, entry.reason),
            fix: None,
            level: "info",
            topic: "dependency-averse-excluded",
        });
    }
    findings
}

/// A root `keel.toml` in a Google `agents-cli` project (an
/// `agents-cli-manifest.yaml` naming an `agent_directory`) never reaches the
/// container: the generated Dockerfile only `COPY`s `pyproject.toml`,
/// `README.md`, `uv.lock*`, and the agent directory itself. Emitted only when
/// a manifest is found, `<project>/keel.toml` actually exists, and the agent
/// directory is not `project` itself — when `agent_directory` names the
/// project root, the root `keel.toml` already sits inside the one directory
/// the Dockerfile ships, so there is no placement problem to report.
fn agents_cli_placement_finding(project: &Path) -> Option<Finding> {
    let layout = agents_cli::find_agents_cli_layout(project)?;
    if layout.agent_dir == project || !evidence::keel_toml(project).exists() {
        return None;
    }
    // Display paths relative to `project` (the common case: `agent_directory`
    // names a subdirectory of the project it's declared in) rather than the
    // absolute filesystem path — keeps the finding's text, and therefore
    // `--json`, reproducible across checkouts instead of embedding wherever
    // this particular clone happens to sit on disk.
    let agent_dir = relative_display(project, &layout.agent_dir);
    let moved_to = relative_display(project, &layout.agent_dir.join("keel.toml"));
    Some(Finding {
        action: format!(
            "Move keel.toml to {moved_to} (or add a `COPY keel.toml` line to the Dockerfile)."
        ),
        detail: format!(
            "This is an agents-cli project — its generated Dockerfile only COPYs \
             pyproject.toml, README.md, uv.lock*, and {agent_dir} into the image, so the \
             keel.toml at the project root never ships to the container."
        ),
        fix: None,
        level: "warn",
        topic: "agents-cli-config-placement",
    })
}

/// `target` relative to `base` when it is actually nested under `base`, else
/// the absolute path unchanged (a manifest found above `project`, or on a
/// different mount — pathological, but must not panic or produce nonsense
/// like `../../../../tmp/x`).
fn relative_display(base: &Path, target: &Path) -> String {
    target.strip_prefix(base).map_or_else(
        |_| target.display().to_string(),
        |rel| rel.display().to_string(),
    )
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

/// Assemble the report from the five evidence inputs. Pure, so the golden test
/// pins it without a filesystem or `python3` — the one filesystem-dependent
/// input (`agents_cli_finding`, since it needs to walk for a manifest and
/// check for a root `keel.toml`) is computed by the caller and passed in
/// already resolved, the same pattern `policy`/`journal` already use.
fn build_report(
    scan: &ScanResult,
    wrapped_targets: &BTreeSet<String>,
    policy: PolicyValidation,
    journal: JournalReport,
    agents_cli_finding: Option<Finding>,
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

    // Topology: sort every sighted host into exactly one of the three honesty
    // buckets, plus the host-independent external-process signal — see
    // [`classify_topology`].
    let topology = classify_topology(scan, wrapped_targets);

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
    findings.extend(topology_findings(&topology));
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
    findings.extend(resilience_finding(scan, &registry_libs));
    findings.extend(journal_finding(&journal));
    findings.extend(agents_cli_finding);

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
        topology,
    }
}

/// Sort every host the static scan saw into exactly one of the three honesty
/// buckets (dx-spec §2 — "wrap it" / "can't reach it" / "shouldn't reach
/// it"), plus the host-independent external-process signal. Precedence: a
/// wrapped-at-runtime target or an `llm:*` target is wrappable by
/// construction regardless of transport class (runtime evidence, or the LLM
/// pack's own wrapping, beats static doubt); otherwise a target seen ONLY
/// inside a dependency-averse file is excluded (shouldn't reach it) ahead of
/// any transport check; otherwise the transport class decides wrappable
/// (tracked) vs. unreachable (untracked-known/unknown). `pub(crate)`:
/// `init.rs` reuses this directly for `keel init --diff` to skip proposing
/// policy for excluded hosts and print why.
pub(crate) fn classify_topology(scan: &ScanResult, wrapped_targets: &BTreeSet<String>) -> Topology {
    let dep_files: BTreeSet<&str> = scan
        .dependency_averse
        .iter()
        .map(|d| d.file.as_str())
        .collect();
    let mut wrappable = Vec::new();
    let mut unreachable = Vec::new();
    let mut excluded = Vec::new();
    for (target, ev) in &scan.targets {
        if wrapped_targets.contains(target) || target.starts_with("llm:") {
            wrappable.push(target.clone());
            continue;
        }
        let only_dep_averse = !ev.sightings.is_empty()
            && ev
                .sightings
                .iter()
                .all(|s| dep_files.contains(s.file.as_str()));
        if only_dep_averse {
            let files: BTreeSet<&str> = ev.sightings.iter().map(|s| s.file.as_str()).collect();
            excluded.push(TopologyEntry {
                host: target.clone(),
                reason: format!(
                    "seen only in dependency-averse file(s) {} — excluded from proposed policy; \
                     add `# keel: include` to override",
                    files.into_iter().collect::<Vec<_>>().join(", ")
                ),
            });
            continue;
        }
        match scan
            .host_transports
            .get(target)
            .copied()
            .unwrap_or(TransportClass::Unknown)
        {
            TransportClass::Tracked => wrappable.push(target.clone()),
            TransportClass::UntrackedKnown => unreachable.push(TopologyEntry {
                host: target.clone(),
                reason: "reached via a transport Keel does not adapt (stdlib urllib/http.client)"
                    .to_owned(),
            }),
            TransportClass::Unknown => unreachable.push(TopologyEntry {
                host: target.clone(),
                reason: "URL literal with no tracked transport in reach — trace how this request \
                          is dispatched"
                    .to_owned(),
            }),
        }
    }
    let external_processes: Vec<ExternalProcess> = scan
        .subprocesses
        .iter()
        .map(|s| ExternalProcess {
            command: s.command.clone(),
            file: s.file.clone(),
            launcher: s.launcher.clone(),
            line: s.line,
        })
        .collect();
    Topology {
        excluded,
        external_processes,
        unreachable,
        wrappable,
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

    out.push_str("\ntopology\n");
    line_list(&mut out, "  wrap it:          ", &r.topology.wrappable);
    for e in &r.topology.unreachable {
        let line = format!("  can't reach:       {} — {}\n", e.host, e.reason);
        out.push_str(&line);
    }
    for e in &r.topology.excluded {
        let line = format!("  shouldn't reach:   {} — {}\n", e.host, e.reason);
        out.push_str(&line);
    }
    for p in &r.topology.external_processes {
        let line = format!(
            "  external process:  {} ({} at {}:{})\n",
            p.command, p.launcher, p.file, p.line
        );
        out.push_str(&line);
    }

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
        let r = build_report(&scan, &wrapped, policy, default_journal(), None);

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
    fn topology_buckets_classify_hosts_honestly() {
        use crate::scan::{DepAverseFile, SubprocessSighting, TransportClass};
        let mut scan = ScanResult {
            files_scanned: 3,
            python_available: true,
            ..ScanResult::default()
        };
        // wrappable: tracked transport.
        scan.targets.insert(
            "api.ok.com".into(),
            TargetEvidence {
                class: TargetClass::Host,
                sightings: [Sighting {
                    file: "app.py".into(),
                    line: 3,
                }]
                .into_iter()
                .collect(),
            },
        );
        scan.host_transports
            .insert("api.ok.com".into(), TransportClass::Tracked);
        // unreachable: untracked-known transport.
        scan.targets.insert(
            "api.stdlib.com".into(),
            TargetEvidence {
                class: TargetClass::Host,
                sightings: [Sighting {
                    file: "screen.py".into(),
                    line: 9,
                }]
                .into_iter()
                .collect(),
            },
        );
        scan.host_transports
            .insert("api.stdlib.com".into(), TransportClass::UntrackedKnown);
        // excluded: sighted ONLY inside a dependency-averse file (transport
        // class irrelevant).
        scan.targets.insert(
            "api.broker.com".into(),
            TargetEvidence {
                class: TargetClass::Host,
                sightings: [Sighting {
                    file: "risk_gate.py".into(),
                    line: 20,
                }]
                .into_iter()
                .collect(),
            },
        );
        scan.host_transports
            .insert("api.broker.com".into(), TransportClass::UntrackedKnown);
        scan.dependency_averse.push(DepAverseFile {
            file: "risk_gate.py".into(),
            reason: "stdlib-only + name/docstring signal: risk".into(),
        });
        scan.subprocesses.push(SubprocessSighting {
            file: "mcp.sh.py".into(),
            line: 12,
            launcher: "subprocess.run".into(),
            command: "uvx alpaca-mcp-server".into(),
        });
        let r = build_report(
            &scan,
            &BTreeSet::new(),
            default_policy(),
            default_journal(),
            None,
        );
        assert_eq!(r.topology.wrappable, vec!["api.ok.com"]);
        assert_eq!(r.topology.unreachable.len(), 1);
        assert_eq!(r.topology.unreachable[0].host, "api.stdlib.com");
        assert_eq!(r.topology.excluded.len(), 1);
        assert_eq!(r.topology.excluded[0].host, "api.broker.com");
        assert!(r.topology.excluded[0].reason.contains("risk_gate.py"));
        assert_eq!(r.topology.external_processes.len(), 1);
        assert_eq!(
            r.topology.external_processes[0].command,
            "uvx alpaca-mcp-server"
        );
        // findings carry the honesty.
        assert!(
            r.findings
                .iter()
                .any(|f| f.topic == "url-no-transport" && f.level == "warn")
        );
        assert!(r.findings.iter().any(|f| f.topic == "subprocess-blind-spot"
            && f.level == "warn"
            && f.detail.contains("uvx alpaca-mcp-server")));
        assert!(
            r.findings
                .iter()
                .any(|f| f.topic == "dependency-averse-excluded" && f.level == "info")
        );
        // ok is unaffected: honesty findings are not configuration errors.
        assert!(r.ok);
    }

    /// The override precedence documented on [`classify_topology`]: a
    /// wrapped-at-runtime target or an `llm:*` target is wrappable by
    /// construction, regardless of what the transport check or the
    /// dependency-averse check would otherwise conclude. Runtime evidence (or
    /// the LLM pack's own wrapping) beats static doubt.
    #[test]
    fn wrapped_and_llm_targets_are_always_wrappable() {
        use crate::scan::{DepAverseFile, TransportClass};
        let mut scan = ScanResult {
            files_scanned: 3,
            python_available: true,
            ..ScanResult::default()
        };
        // Would otherwise be unreachable (Unknown transport) if not wrapped.
        scan.targets.insert(
            "api.wrapped-but-unknown.com".into(),
            TargetEvidence {
                class: TargetClass::Host,
                sightings: [Sighting {
                    file: "app.py".into(),
                    line: 1,
                }]
                .into_iter()
                .collect(),
            },
        );
        scan.host_transports.insert(
            "api.wrapped-but-unknown.com".into(),
            TransportClass::Unknown,
        );
        // Would otherwise be excluded (dependency-averse-only-sighted) if not
        // wrapped.
        scan.targets.insert(
            "api.wrapped-but-excluded.com".into(),
            TargetEvidence {
                class: TargetClass::Host,
                sightings: [Sighting {
                    file: "risk_gate.py".into(),
                    line: 5,
                }]
                .into_iter()
                .collect(),
            },
        );
        scan.host_transports.insert(
            "api.wrapped-but-excluded.com".into(),
            TransportClass::UntrackedKnown,
        );
        scan.dependency_averse.push(DepAverseFile {
            file: "risk_gate.py".into(),
            reason: "stdlib-only + name/docstring signal: risk".into(),
        });
        // An llm:* target with no transport evidence at all — wrappable by
        // construction, not by the transport map.
        scan.targets.insert(
            "llm:some-model".into(),
            TargetEvidence {
                class: TargetClass::Llm,
                sightings: [Sighting {
                    file: "agent.py".into(),
                    line: 7,
                }]
                .into_iter()
                .collect(),
            },
        );
        let wrapped: BTreeSet<String> = [
            "api.wrapped-but-unknown.com".to_owned(),
            "api.wrapped-but-excluded.com".to_owned(),
        ]
        .into_iter()
        .collect();
        let r = build_report(&scan, &wrapped, default_policy(), default_journal(), None);

        assert!(
            r.topology
                .wrappable
                .contains(&"api.wrapped-but-unknown.com".to_owned()),
            "a wrapped-at-runtime target must be wrappable even with an Unknown transport class: \
             {:?}",
            r.topology
        );
        assert!(
            !r.topology
                .unreachable
                .iter()
                .any(|e| e.host == "api.wrapped-but-unknown.com"),
            "must not also land in unreachable"
        );
        assert!(
            r.topology
                .wrappable
                .contains(&"api.wrapped-but-excluded.com".to_owned()),
            "a wrapped-at-runtime target must be wrappable even when sighted only in a \
             dependency-averse file: {:?}",
            r.topology
        );
        assert!(
            !r.topology
                .excluded
                .iter()
                .any(|e| e.host == "api.wrapped-but-excluded.com"),
            "must not also land in excluded"
        );
        assert!(
            r.topology.wrappable.contains(&"llm:some-model".to_owned()),
            "an llm:* target must be wrappable with zero transport evidence: {:?}",
            r.topology
        );
    }

    /// The six agent-framework packs + google-genai are registered adapters:
    /// detected, pinned, and their `target` matches each pack's own declared
    /// `TargetDecl.pattern` — so importing them is coverage, not an
    /// "invisible" finding.
    #[test]
    fn agent_pack_adapters_are_registered_pinned_and_detected() {
        let scan = scan_with(
            "llm:google-genai",
            TargetClass::Llm,
            &[
                "google-adk",
                "google-genai",
                "pydantic-ai",
                "openai-agents",
                "crewai",
                "langgraph",
                "mcp",
            ],
        );
        let policy = PolicyValidation {
            check: PolicyCheck {
                field: None,
                message: None,
                present: false,
                valid: true,
            },
            fix: None,
        };
        let r = build_report(&scan, &BTreeSet::new(), policy, default_journal(), None);

        assert!(
            r.coverage.invisible.is_empty(),
            "every imported agent-pack lib has a registry adapter: {:?}",
            r.coverage.invisible
        );
        for (lib, target) in [
            ("google-adk", "tool:<name>"),
            ("google-genai", "llm:google-genai"),
            ("pydantic-ai", "tool:<name>"),
            ("openai-agents", "tool:<name>"),
            ("crewai", "tool:<name>"),
            ("langgraph", "tool:<name>"),
            // `mcp` is a single REGISTRY row shared by Python and Node (like
            // openai/anthropic above): one flat `scan.libs` detection covers
            // both, so it belongs in this same table-driven loop rather than
            // a separate assertion block.
            ("mcp", "mcp:<server>"),
        ] {
            let a = r
                .adapters
                .iter()
                .find(|a| a.lib == lib && a.target == target)
                .unwrap_or_else(|| panic!("missing REGISTRY entry for {lib} -> {target}"));
            assert!(a.detected, "{lib} should be detected");
            assert_eq!(a.status, "pinned");
        }
    }

    fn default_policy() -> PolicyValidation {
        PolicyValidation {
            check: PolicyCheck {
                field: None,
                message: None,
                present: false,
                valid: true,
            },
            fix: None,
        }
    }

    #[test]
    fn resilience_lib_alongside_a_wrapped_effect_is_a_finding() {
        let mut scan = scan_with("api.example.com", TargetClass::Host, &["httpx"]);
        scan.resilience_libs.insert("tenacity".to_owned());
        let r = build_report(
            &scan,
            &BTreeSet::new(),
            default_policy(),
            default_journal(),
            None,
        );
        let finding = r
            .findings
            .iter()
            .find(|f| f.topic == "preexisting-resilience")
            .expect("tenacity + httpx should raise a finding");
        assert_eq!(finding.level, "warn");
        assert!(finding.detail.contains("tenacity"));
    }

    #[test]
    fn resilience_lib_with_no_wrapped_effect_is_not_a_finding() {
        // tenacity imported, but nothing Keel would ever wrap alongside it —
        // no evidence of compounding, so no finding (avoids the false
        // positive of flagging an unrelated/unused import).
        let mut scan = ScanResult {
            files_scanned: 1,
            python_available: true,
            ..ScanResult::default()
        };
        scan.resilience_libs.insert("tenacity".to_owned());
        let r = build_report(
            &scan,
            &BTreeSet::new(),
            default_policy(),
            default_journal(),
            None,
        );
        assert!(
            !r.findings
                .iter()
                .any(|f| f.topic == "preexisting-resilience")
        );
    }

    #[test]
    fn no_resilience_libs_is_not_a_finding() {
        let scan = scan_with("api.example.com", TargetClass::Host, &["httpx"]);
        let r = build_report(
            &scan,
            &BTreeSet::new(),
            default_policy(),
            default_journal(),
            None,
        );
        assert!(
            !r.findings
                .iter()
                .any(|f| f.topic == "preexisting-resilience")
        );
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
        let r = build_report(&scan, &wrapped, policy, default_journal(), None);
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
        let r = build_report(&scan, &wrapped, policy, journal, None);
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

    // ---- agents-cli config placement ----

    /// A manifest naming an agent directory other than the project root, plus
    /// a root `keel.toml`, is exactly the layout that never ships: the finding
    /// fires with the Dockerfile explanation and a move-it action.
    #[test]
    fn agents_cli_placement_finding_fires_for_a_root_keel_toml() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("app")).unwrap();
        std::fs::write(
            dir.path().join("agents-cli-manifest.yaml"),
            "agent_directory: app\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("keel.toml"), "[target.\"x\"]\n").unwrap();

        let finding =
            agents_cli_placement_finding(dir.path()).expect("root keel.toml should be flagged");
        assert_eq!(finding.level, "warn");
        assert_eq!(finding.topic, "agents-cli-config-placement");
        assert!(finding.detail.contains("Dockerfile"));
        assert!(finding.detail.contains("pyproject.toml"));
        assert!(finding.detail.contains("app"));
        assert!(
            finding.action.contains("app/keel.toml") || finding.action.contains("app\\keel.toml"),
            "action names the relative move-to path: {}",
            finding.action
        );
    }

    /// No manifest at all: never a finding, regardless of a root `keel.toml`.
    #[test]
    fn agents_cli_placement_finding_is_none_without_a_manifest() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("keel.toml"), "[target.\"x\"]\n").unwrap();
        assert!(agents_cli_placement_finding(dir.path()).is_none());
    }

    /// The `keel.toml` already lives in the agent directory (the correct
    /// place) and the project root has none: nothing to flag.
    #[test]
    fn agents_cli_placement_finding_is_none_when_keel_toml_is_already_in_the_agent_dir() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("app")).unwrap();
        std::fs::write(
            dir.path().join("agents-cli-manifest.yaml"),
            "agent_directory: app\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("app").join("keel.toml"), "[target.\"x\"]\n").unwrap();

        assert!(agents_cli_placement_finding(dir.path()).is_none());
    }

    /// A manifest whose `agent_directory` names the project root itself: the
    /// root `keel.toml` already sits inside the one directory the Dockerfile
    /// ships, so there is nothing to flag.
    #[test]
    fn agents_cli_placement_finding_is_none_when_agent_dir_is_the_project_root() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("agents-cli-manifest.yaml"),
            "agent_directory: .\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("keel.toml"), "[target.\"x\"]\n").unwrap();

        assert!(agents_cli_placement_finding(dir.path()).is_none());
    }

    /// End-to-end through `run()`: the finding surfaces in the full report and
    /// (being a warn, not an error) does not flip `ok` to false.
    #[test]
    fn doctor_run_surfaces_the_agents_cli_placement_finding() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("app")).unwrap();
        std::fs::write(
            dir.path().join("agents-cli-manifest.yaml"),
            "agent_directory: app\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("keel.toml"), "[target.\"x\"]\n").unwrap();

        let r = run(dir.path());
        assert_eq!(r.exit, EXIT_OK);
        assert_eq!(r.json["ok"], true);
        let findings = r.json["findings"].as_array().unwrap();
        assert!(
            findings
                .iter()
                .any(|f| f["topic"] == "agents-cli-config-placement" && f["level"] == "warn")
        );
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
