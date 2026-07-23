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

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;

use keel_core_api::policy::{FlowMatchRule, Policy};
use serde::Serialize;

use crate::cmd_match::{compile_cmd_rules, match_argv};
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
    // The stdlib urllib.request pack (WS4). Convention exception, documented
    // here on the registry itself: stdlib has no pip version to pin, so this
    // row is keyed to the PYTHON RUNTIME version — the pack's detect()
    // reports platform.python_version() and certifies the interpreter lines
    // in urllib_pack._PINNED (CI pins 3.11). "pinned", not best-effort: the
    // seam is a stable stdlib API certified per interpreter line by the farm.
    Adapter {
        lib: "urllib.request",
        lang: "python",
        target: "host",
        best_effort: false,
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

/// The registry's library names, for cross-module gates that must not drift
/// from doctor's own (init's pre-existing-resilience annotation uses the
/// same "imports at least one lib Keel wraps" test as [`resilience_finding`]).
pub(crate) fn registry_libs() -> BTreeSet<&'static str> {
    REGISTRY.iter().map(|a| a.lib).collect()
}

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

/// One ranked follow-up: a lead Keel cannot chase itself, phrased for the
/// agent/human reading the report to work top-down. `code` is a CLOSED set —
/// url-no-transport | orchestration-blind-spot | subprocess-blind-spot |
/// dependency-averse-excluded | preexisting-resilience | code-hash-stale —
/// ranked lowest-Keel-confidence first (rank 1 = Keel knows least, investigate
/// first). Text is entirely keel-authored; only hostnames, file paths, and lib
/// names are interpolated.
#[derive(Debug, Serialize)]
struct FollowUp {
    code: &'static str,
    detail: String,
    rank: u32,
    subject: String,
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
    /// The `cmd:<name>` entrypoint this sighting's argv matches under the
    /// project's declared `[flows.match."cmd:*"]` rules (issue #41), or
    /// `None` when unmatched (or the launcher isn't one the runtime pack
    /// ever intercepts, or its argv is not a genuine positional literal —
    /// see [`scan::SubprocessSighting::argv`]). A match means "wrapped WHEN
    /// Keel is active in the process that runs it" — doctor cannot know
    /// activation from a static scan, so a match downgrades this finding
    /// rather than dropping it; see [`topology_findings`]/[`build_follow_ups`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) covered_by: Option<String>,
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

/// What this report could and could not read — the honest frame around every
/// other field, and the one place a consumer that called `keel doctor --json`
/// (or the `get_doctor_report` MCP tool) *alone* learns that the report is
/// evidence, not a verdict.
///
/// Deliberately NOT a [`Finding`]: findings are things to act on, they feed the
/// human findings list and (via structured evidence) `follow_ups`, and an
/// unconditional advisory in that list is exactly the kind of false positive
/// [`resilience_finding`] argues erodes trust in the real ones. These are
/// standing properties of the tool instead. The MCP surface is contractually
/// byte-identical to `keel doctor --json`, so there is no agent-only channel —
/// anything an agent must read has to live here, in the shared report.
///
/// `governance_files` is filesystem-dependent, so the whole object is built in
/// [`run`] and passed into the pure, golden-pinned [`build_report`] — the same
/// pattern `agents_cli_finding`/`stale_flows` already use.
#[derive(Debug, Serialize)]
struct Boundaries {
    /// Root files carrying project constraints this report cannot parse —
    /// `CLAUDE.md`, `AGENTS.md`. Empty when neither exists. Edit deny-lists,
    /// fail-closed contracts and "never touch this file" rules live in that
    /// prose, so a call site that looks wrappable may be deliberately locked.
    governance_files: Vec<&'static str>,
    /// The languages the static scan parses into an AST.
    parsed_languages: &'static [&'static str],
    /// One line naming the protocol that turns this evidence into a verdict,
    /// for an agent that reached the tool without the skill.
    protocol: &'static str,
    /// File classes this report never parses. Shell/`Makefile`/CI files are
    /// sighted coarsely by substring (see the `orchestration-blind-spot`
    /// finding); governance prose is not read at all.
    unparsed: &'static [&'static str],
}

/// The whole doctor report.
#[derive(Debug, Serialize)]
struct DoctorReport {
    adapters: Vec<AdapterStatus>,
    boundaries: Boundaries,
    coverage: Coverage,
    findings: Vec<Finding>,
    follow_ups: Vec<FollowUp>,
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
    /// The declared `[flows.match."cmd:*"]` table (issue #41), so
    /// `classify_topology` can cross-reference subprocess sightings — empty
    /// when `keel.toml` is absent, invalid, or simply declares no rules
    /// (the honest default: no rules means no sighting is ever "covered").
    cmd_match: BTreeMap<String, FlowMatchRule>,
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
    let registry_libs = registry_libs();
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
    let boundaries = boundaries(project);
    let stale_flows = crate::flows::stale_code_hash_flows(project);
    let report = build_report(
        &scan,
        &discovery,
        policy,
        journal,
        agents_cli_finding,
        boundaries,
        &stale_flows,
    );
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
/// positive that erodes trust in doctor's other findings. Detected in both
/// languages: `scan::python`'s `RESILIENCE_LIBS` (tenacity/backoff/
/// retrying/stamina) and `scan::js`'s equivalent (p-retry/async-retry) —
/// `got`'s built-in `retry` option is a different, non-import-based signal
/// and not covered here.
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
            action: "Trace how this request is actually dispatched before proposing policy; \
                      Python's stdlib `urllib.request` is adapted — importing it directly in \
                      that file makes the host wrappable."
                .to_owned(),
            detail: format!("`{}` — {}.", entry.host, entry.reason),
            fix: None,
            level: "warn",
            topic: "url-no-transport",
        });
    }
    let (covered, uncovered): (Vec<_>, Vec<_>) = topology
        .external_processes
        .iter()
        .partition(|p| p.covered_by.is_some());
    if !uncovered.is_empty() {
        let cmds: Vec<String> = uncovered
            .iter()
            .map(|p| format!("`{}` ({} at {}:{})", p.command, p.launcher, p.file, p.line))
            .collect();
        findings.push(Finding {
            action: "Confirm none of these processes carry traffic you care about; Keel must be \
                      installed inside a process to see it."
                .to_owned(),
            detail: format!(
                "Keel cannot see traffic inside {} externally-launched process(es): {}.",
                uncovered.len(),
                cmds.join(", ")
            ),
            fix: None,
            level: "warn",
            topic: "subprocess-blind-spot",
        });
    }
    if !covered.is_empty() {
        // Issue #41: a sighting matching a declared `[flows.match."cmd:*"]`
        // rule is wrapped WHEN Keel is active in the process that runs it —
        // a static scan cannot confirm activation, so this downgrades to
        // `info` rather than dropping the sighting (overclaiming coverage
        // doctor can't verify would be its own honesty violation).
        let cmds: Vec<String> = covered
            .iter()
            .map(|p| {
                format!(
                    "`{}` ({} at {}:{}, matches `{}`)",
                    p.command,
                    p.launcher,
                    p.file,
                    p.line,
                    p.covered_by.as_deref().unwrap_or_default()
                )
            })
            .collect();
        findings.push(Finding {
            action: "No action needed unless the matching `[flows.match]` rule is wrong, or Keel \
                      is not actually active in the process that runs this command."
                .to_owned(),
            detail: format!(
                "{} externally-launched process(es) match a declared `[flows.match.\"cmd:*\"]` \
                 rule and are wrapped when Keel is active in that process: {}.",
                covered.len(),
                cmds.join(", ")
            ),
            fix: None,
            level: "info",
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

/// The WS3 simplification findings: each hand-rolled pattern the scan
/// sighted inside a target-reaching function becomes one paired finding —
/// "here is the target; once Keel wraps it, the code at file:line is
/// redundant". The level pairs with the topology bucket: `warn` when any of
/// the sighting's targets is already wrappable (deleting the pattern is
/// actionable now), `info` when wrapping itself is still pending (e.g. a
/// stdlib-urllib transport before the urllib pack lands). Never affects
/// `ok`. Interpolates only hosts, file paths, line numbers, and function
/// names (the no-raw-source hardening rule).
fn simplification_findings(scan: &ScanResult, topology: &Topology) -> Vec<Finding> {
    let wrappable: BTreeSet<&str> = topology.wrappable.iter().map(String::as_str).collect();
    let mut findings = Vec::new();
    for s in &scan.simplifications {
        let targets = s.targets.join(", ");
        let actionable_now = s.targets.iter().any(|t| wrappable.contains(t.as_str()));
        let (level, when) = if actionable_now {
            ("warn", "Keel can wrap this target now")
        } else {
            ("info", "once Keel can wrap this target")
        };
        let (topic, what, action): (&'static str, String, String) = match s.kind.as_str() {
            "hand-rolled-poll" => (
                "hand-rolled-poll",
                format!(
                    "`{}` ({}:{}) hand-rolls poll-until-terminal against `{}` — {}, a `poll` \
                     policy (interval / deadline / until) replaces the whole loop",
                    s.function, s.file, s.line, targets, when
                ),
                "Wrap the target, then replace the loop with a `poll` policy — the poll \
                 primitive (CCR-3) is the designated replacement for submit-then-poll loops."
                    .to_owned(),
            ),
            "silent-swallow" => (
                "silent-swallow",
                format!(
                    "`{}` ({}:{}) silences failures from `{}` with a broad `except` returning a \
                     default — Keel replaces silence with retry + observability",
                    s.function, s.file, s.line, targets
                ),
                "Wrap the target so Keel's policy owns the failure (retry, breaker, journal), \
                 then narrow or remove the broad except."
                    .to_owned(),
            ),
            "hand-rolled-retry" => (
                "hand-rolled-retry",
                format!(
                    "`{}` ({}:{}) hand-rolls retry around `{}` — {}, this loop becomes redundant",
                    s.function, s.file, s.line, targets, when
                ),
                "Wrap the target with Keel retry/backoff policy, then delete the hand-rolled \
                 loop — don't run both."
                    .to_owned(),
            ),
            other => {
                // The scanner is the only producer of `simplifications`, and it emits
                // exactly the three kinds matched above — an unrecognized kind is a
                // scanner/doctor drift bug, not a real finding to mislabel and report.
                // Fail loud where it's cheap to catch (debug/test builds); in release,
                // skip rather than surface a wrong action.
                debug_assert!(false, "unknown simplification kind: {other}");
                continue;
            }
        };
        findings.push(Finding {
            action,
            detail: format!("{what}."),
            fix: None,
            level,
            topic,
        });
    }
    findings
}

/// The rank table: ascending Keel-confidence. Rank 1 (url-no-transport) is
/// the claim Keel knows least about — it saw a URL but cannot even name the
/// dispatch path — so it is investigated first. Rank 2
/// (orchestration-blind-spot) is a coarse substring match on a file Keel
/// cannot parse at all — strictly less verifiable than rank 3
/// (subprocess-blind-spot), which comes from an AST sighting of a real call
/// — so it sorts above it. Rank 6 (code-hash-stale, reserved for the WS6
/// emitter) is a mechanical, fully-verified fact that merely awaits a human
/// decision.
fn follow_up_rank(code: &str) -> u32 {
    match code {
        "url-no-transport" => 1,
        "orchestration-blind-spot" => 2,
        "subprocess-blind-spot" => 3,
        "dependency-averse-excluded" => 4,
        "preexisting-resilience" => 5,
        _ => 6, // code-hash-stale (WS6)
    }
}

/// The ranked follow-up list (WS2): computed from the same structured
/// evidence as the findings — never by parsing finding text — then sorted by
/// the stable key (rank, code, subject).
fn build_follow_ups(
    topology: &Topology,
    resilience: Option<&Finding>,
    scan: &ScanResult,
    stale_flows: &[crate::flows::StaleFlow],
) -> Vec<FollowUp> {
    let mut ups = Vec::new();
    for entry in &topology.unreachable {
        ups.push(FollowUp {
            code: "url-no-transport",
            detail: format!(
                "Trace how requests to `{}` are actually dispatched — {}.",
                entry.host, entry.reason
            ),
            rank: follow_up_rank("url-no-transport"),
            subject: entry.host.clone(),
        });
    }
    if !scan.orchestration.is_empty() {
        let mut files: Vec<&str> = scan.orchestration.iter().map(|o| o.file.as_str()).collect();
        files.dedup();
        ups.push(FollowUp {
            code: "orchestration-blind-spot",
            detail: "Keel cannot parse shell/Makefile/CI files; these carry a coarse \
                     at-most-once-dispatch signature (lockfile/guard/PID check). Confirm \
                     whether each is real dispatch gating a `cmd:` flow could replace."
                .to_owned(),
            rank: follow_up_rank("orchestration-blind-spot"),
            subject: format!("{} orchestration file(s)", files.len()),
        });
    }
    // Issue #41: a sighting matching a declared `[flows.match."cmd:*"]` rule
    // is covered when Keel is active, so it drops out of this "investigate
    // top-down" list entirely — it needs no chasing, only the lower-priority
    // `info` finding `topology_findings` still emits for it.
    let uncovered: Vec<&ExternalProcess> = topology
        .external_processes
        .iter()
        .filter(|p| p.covered_by.is_none())
        .collect();
    if !uncovered.is_empty() {
        let cmds: Vec<String> = uncovered
            .iter()
            .map(|p| format!("`{}` ({}:{})", p.command, p.file, p.line))
            .collect();
        ups.push(FollowUp {
            code: "subprocess-blind-spot",
            detail: format!(
                "Keel cannot see traffic inside externally-launched processes; confirm none of \
                 these carry traffic you care about: {}.",
                cmds.join(", ")
            ),
            rank: follow_up_rank("subprocess-blind-spot"),
            subject: format!("{} externally-launched process(es)", uncovered.len()),
        });
    }
    for entry in &topology.excluded {
        ups.push(FollowUp {
            code: "dependency-averse-excluded",
            detail: format!(
                "`{}` was excluded from proposed policy — {}. Confirm the exclusion is intended.",
                entry.host, entry.reason
            ),
            rank: follow_up_rank("dependency-averse-excluded"),
            subject: entry.host.clone(),
        });
    }
    if resilience.is_some() {
        let libs: Vec<&str> = scan.resilience_libs.iter().map(String::as_str).collect();
        ups.push(FollowUp {
            code: "preexisting-resilience",
            detail: format!(
                "Decide whether {} still needs its own retry/backoff now that Keel wraps the \
                 same calls — delete the old code or scope Keel's policy, not both.",
                libs.join(", ")
            ),
            rank: follow_up_rank("preexisting-resilience"),
            subject: libs.join(", "),
        });
    }
    for flow in stale_flows {
        ups.push(FollowUp {
            code: "code-hash-stale",
            detail: format!(
                "Flow `{}` ({}) was recorded under a different code hash than its current \
                 script; resuming would replay recorded steps against changed code (the resume \
                 fence downgrades nondeterminism fail->warn). Inspect with `keel replay {}` \
                 before resuming.",
                flow.flow_id, flow.entrypoint, flow.flow_id
            ),
            rank: follow_up_rank("code-hash-stale"),
            subject: flow.flow_id.clone(),
        });
    }
    ups.sort_by(|a, b| {
        (a.rank, a.code, a.subject.as_str()).cmp(&(b.rank, b.code, b.subject.as_str()))
    });
    ups
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

/// Build the [`Boundaries`] frame for `project`. Only `governance_files` touches
/// the filesystem; the rest are standing properties of this tool, kept in one
/// place so there is a single edit when the scan learns a new language or file
/// class. The `protocol` line enumerates the skill's five phases verbatim — if
/// `skills/keel/SKILL.md`'s protocol changes, change this with it.
fn boundaries(project: &Path) -> Boundaries {
    let mut governance_files = Vec::new();
    if project.join("CLAUDE.md").exists() {
        governance_files.push("CLAUDE.md");
    }
    if project.join("AGENTS.md").exists() {
        governance_files.push("AGENTS.md");
    }
    Boundaries {
        governance_files,
        parsed_languages: &["python", "js-ts"],
        protocol: "Static + adapter-interception evidence, not a verdict. Drive an \
                   evaluate/adopt/review task through the keel skill's five phases: Scope every \
                   I/O process (including shell/CI launchers) -> Explore how each call is \
                   dispatched -> Collect this report -> Baseline real failure classes in observe \
                   mode (`keel record run`) -> Analyze & propose. Retry only helps \
                   genuinely-transient classes (conn/timeout/5xx/429).",
        unparsed: &["shell", "makefile", "ci-workflow", "governance-prose"],
    }
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

/// Assemble the report from the seven evidence inputs. Pure, so the golden test
/// pins it without a filesystem or `python3` — the filesystem-dependent
/// inputs (`agents_cli_finding`, since it needs to walk for a manifest and
/// check for a root `keel.toml`; `boundaries`, since it stats the project root
/// for governance files; `stale_flows`, since it needs to read
/// `.keel/journal.db` and stat scripts on disk) are computed by the caller
/// and passed in already resolved, the same pattern `policy`/`journal`
/// already use.
#[allow(clippy::too_many_lines)] // straight-line report assembly, one section per
// DoctorReport field; issue #41 added the cmd_match plumbing, not new complexity.
fn build_report(
    scan: &ScanResult,
    wrapped_targets: &BTreeSet<String>,
    policy: PolicyValidation,
    journal: JournalReport,
    agents_cli_finding: Option<Finding>,
    boundaries: Boundaries,
    stale_flows: &[crate::flows::StaleFlow],
) -> DoctorReport {
    let PolicyValidation {
        check: policy,
        cmd_match,
        fix,
    } = policy;
    let registry_libs = registry_libs();

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
    let topology = classify_topology(scan, wrapped_targets, &cmd_match);

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
    // Conditional: unparsed orchestration files that hand-roll at-most-once
    // dispatch. A lead, not a verdict — the scan cannot parse these files, so
    // the finding names where to look and never claims what it found.
    if !scan.orchestration.is_empty() {
        const MAX_LISTED: usize = 5;
        let mut files: Vec<&str> = scan.orchestration.iter().map(|o| o.file.as_str()).collect();
        // `scan.orchestration` is sorted by (file, line, kind), so same-file
        // entries are adjacent and `dedup` is exact.
        files.dedup();
        let shown = files.len().min(MAX_LISTED);
        let mut list = files[..shown]
            .iter()
            .map(|f| format!("`{f}`"))
            .collect::<Vec<_>>()
            .join(", ");
        if files.len() > shown {
            let rest = files.len() - shown;
            let _ = write!(list, " and {rest} more");
        }
        findings.push(Finding {
            action: "Inspect these files for hand-rolled at-most-once dispatch (lockfile/guard/\
                     PID checks). A durable `cmd:` flow replaces it crash-safely: `keel exec \
                     --flow` for a standalone launcher, or `[flows.match.\"cmd:<name>\"]` when \
                     the call is made from inside an already-Keel-active process."
                .to_owned(),
            detail: format!(
                "Static scan cannot parse these orchestration files, but sighted the \
                 at-most-once-dispatch signature in: {list}."
            ),
            fix: None,
            level: "warn",
            topic: "orchestration-blind-spot",
        });
    }
    findings.extend(topology_findings(&topology));
    findings.extend(simplification_findings(scan, &topology));
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
    let resilience = resilience_finding(scan, &registry_libs);
    let follow_ups = build_follow_ups(&topology, resilience.as_ref(), scan, stale_flows);
    findings.extend(resilience);
    findings.extend(journal_finding(&journal));
    findings.extend(agents_cli_finding);

    let ok = (policy.valid || !policy.present) && journal.supported;
    DoctorReport {
        adapters,
        boundaries,
        coverage: Coverage {
            invisible,
            visible_unwrapped,
            wrapped,
        },
        findings,
        follow_ups,
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
/// policy for excluded hosts and print why (passing an empty `cmd_match` —
/// `--diff` never touches `external_processes`, so cross-referencing it
/// there would be dead work).
pub(crate) fn classify_topology(
    scan: &ScanResult,
    wrapped_targets: &BTreeSet<String>,
    cmd_match: &BTreeMap<String, FlowMatchRule>,
) -> Topology {
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
                reason: "reached via a stdlib transport Keel does not adapt (http.client, or \
                         urllib without urllib.request; Python's urllib.request itself is \
                         adapted)"
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
    let cmd_rules = compile_cmd_rules(cmd_match);
    let external_processes: Vec<ExternalProcess> = scan
        .subprocesses
        .iter()
        .map(|s| ExternalProcess {
            command: s.command.clone(),
            covered_by: cmd_flow_covering(&cmd_rules, s),
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

/// The launcher names `python/keel/src/keel/adapters/subprocess_pack.py`'s
/// runtime interceptor actually wraps (issue #41's "Coverage" section):
/// `subprocess.run`/`check_output`/`call`/`check_call`, patched directly or
/// via a same-module call the patched name resolves. Deliberately excludes
/// `subprocess.Popen` (the scanner sights it — see `SUBPROC_NAMES` — but the
/// pack never patches it) and `os.system`/`os.popen` (a different launch
/// shape the pack's own docs say it never matches). Node's launchers are
/// never in this list: `SubprocessSighting::argv` is always `None` for a JS
/// sighting today (see `record_subprocess`'s doc), so the `argv.is_some()`
/// gate below already excludes them; this list is the second, explicit gate
/// so that invariant isn't the ONLY thing standing between a scanner change
/// and a false "covered" claim.
const INTERCEPTED_CMD_LAUNCHERS: &[&str] = &[
    "subprocess.run",
    "subprocess.check_output",
    "subprocess.call",
    "subprocess.check_call",
];

/// The `cmd:<name>` entrypoint `sighting` is covered by, or `None` — issue
/// #41. A sighting is only ever a match candidate when its launcher is one
/// the runtime pack actually intercepts AND the scanner captured a genuine
/// positional argv (`argv.is_some()`; see [`scan::SubprocessSighting::argv`]'s
/// doc for the exact conditions — list/tuple of literals, no `shell=True`).
fn cmd_flow_covering(
    rules: &[crate::cmd_match::CompiledCmdRule],
    sighting: &scan::SubprocessSighting,
) -> Option<String> {
    if !INTERCEPTED_CMD_LAUNCHERS.contains(&sighting.launcher.as_str()) {
        return None;
    }
    let argv = sighting.argv.as_ref()?;
    match_argv(rules, argv).map(str::to_owned)
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
            cmd_match: BTreeMap::new(),
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
        Ok(policy) => PolicyValidation {
            check: PolicyCheck {
                field: None,
                message: None,
                present: true,
                valid: true,
            },
            cmd_match: policy.flows.and_then(|f| f.match_).unwrap_or_default(),
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
        cmd_match: BTreeMap::new(),
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
#[allow(clippy::too_many_lines)] // straight-line rendering, one section per
// DoctorReport field; the boundaries section added a few lines, not new complexity.
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
    if !r.follow_ups.is_empty() {
        out.push_str("\nfollow-ups (work top-down; 1 = Keel knows least)\n");
        for f in &r.follow_ups {
            let line = format!("  {}. [{}] {} — {}\n", f.rank, f.code, f.subject, f.detail);
            out.push_str(&line);
        }
    }
    out.push_str("\nboundaries\n");
    let parsed = format!(
        "  parsed:            {} — {} not parsed (shell/Makefile/CI sighted coarsely only)\n",
        r.boundaries.parsed_languages.join(", "),
        r.boundaries.unparsed.join(", "),
    );
    out.push_str(&parsed);
    if !r.boundaries.governance_files.is_empty() {
        let gov = format!(
            "  governance:        {} — read before applying policy; this report can't parse it\n",
            r.boundaries.governance_files.join(", "),
        );
        out.push_str(&gov);
    }
    out.push_str(
        "  next:              evidence, not a verdict — see the keel skill's evaluation protocol\n",
    );

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
            cmd_match: BTreeMap::new(),
            fix: None,
        };
        let r = build_report(
            &scan,
            &wrapped,
            policy,
            default_journal(),
            None,
            empty_boundaries(),
            &[],
        );

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
            argv: Some(vec!["uvx".into(), "alpaca-mcp-server".into()]),
        });
        let r = build_report(
            &scan,
            &BTreeSet::new(),
            default_policy(),
            default_journal(),
            None,
            empty_boundaries(),
            &[],
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

    /// Issue #41: a subprocess sighting whose launcher/argv the runtime pack
    /// actually intercepts, and whose argv matches a declared
    /// `[flows.match."cmd:*"]` rule, is downgraded (info, `covered_by` set,
    /// excluded from the rank-3 follow-up) rather than nagged about — while
    /// an unmatched sighting alongside it keeps the full `warn` + follow-up
    /// treatment `topology_buckets_classify_hosts_honestly` already pins.
    #[test]
    fn covered_subprocess_sighting_is_downgraded_not_dropped() {
        use crate::scan::SubprocessSighting;
        let mut scan = ScanResult {
            files_scanned: 1,
            python_available: true,
            ..ScanResult::default()
        };
        scan.subprocesses.push(SubprocessSighting {
            file: "etl.py".into(),
            line: 9,
            launcher: "subprocess.run".into(),
            command: "etl run".into(),
            argv: Some(vec!["etl".into(), "run".into()]),
        });
        scan.subprocesses.push(SubprocessSighting {
            file: "backup.py".into(),
            line: 20,
            launcher: "subprocess.run".into(),
            command: "backup now".into(),
            argv: Some(vec!["backup".into(), "now".into()]),
        });
        let mut policy = default_policy();
        policy.check.present = true;
        policy.cmd_match.insert(
            "cmd:etl".to_owned(),
            FlowMatchRule {
                argv: vec!["etl".into(), "run".into()],
            },
        );
        let r = build_report(
            &scan,
            &BTreeSet::new(),
            policy,
            default_journal(),
            None,
            empty_boundaries(),
            &[],
        );

        assert_eq!(r.topology.external_processes.len(), 2);
        let etl = r
            .topology
            .external_processes
            .iter()
            .find(|p| p.command == "etl run")
            .expect("etl sighting present");
        assert_eq!(etl.covered_by.as_deref(), Some("cmd:etl"));
        let backup = r
            .topology
            .external_processes
            .iter()
            .find(|p| p.command == "backup now")
            .expect("backup sighting present");
        assert_eq!(backup.covered_by, None);

        // The covered sighting gets an `info` finding naming the match...
        assert!(r.findings.iter().any(|f| f.topic == "subprocess-blind-spot"
            && f.level == "info"
            && f.detail.contains("etl run")
            && f.detail.contains("cmd:etl")));
        // ...the uncovered one keeps the full `warn` finding...
        assert!(r.findings.iter().any(|f| f.topic == "subprocess-blind-spot"
            && f.level == "warn"
            && f.detail.contains("backup now")
            && !f.detail.contains("etl run")));
        // ...and only the uncovered one counts toward the rank-3 follow-up.
        let follow_up = r
            .follow_ups
            .iter()
            .find(|u| u.code == "subprocess-blind-spot")
            .expect("one uncovered sighting still yields a follow-up");
        assert!(follow_up.detail.contains("backup now"));
        assert!(!follow_up.detail.contains("etl run"));
        assert_eq!(follow_up.subject, "1 externally-launched process(es)");
    }

    /// Issue #41: `os.system`/`os.popen` sightings, and `subprocess.Popen`
    /// sightings, are NEVER match candidates even with argv text that would
    /// otherwise match a declared rule — the runtime pack never intercepts
    /// those launchers at all (`subprocess_pack.py`'s "Coverage" section).
    #[test]
    fn uncovered_launchers_never_match_even_with_matching_text() {
        use crate::scan::SubprocessSighting;
        let mut scan = ScanResult {
            files_scanned: 1,
            python_available: true,
            ..ScanResult::default()
        };
        scan.subprocesses.push(SubprocessSighting {
            file: "legacy.py".into(),
            line: 4,
            launcher: "os.system".into(),
            command: "etl run".into(),
            argv: None,
        });
        scan.subprocesses.push(SubprocessSighting {
            file: "legacy.py".into(),
            line: 8,
            launcher: "subprocess.Popen".into(),
            command: "etl run".into(),
            argv: Some(vec!["etl".into(), "run".into()]),
        });
        let mut policy = default_policy();
        policy.check.present = true;
        policy.cmd_match.insert(
            "cmd:etl".to_owned(),
            FlowMatchRule {
                argv: vec!["etl".into(), "run".into()],
            },
        );
        let r = build_report(
            &scan,
            &BTreeSet::new(),
            policy,
            default_journal(),
            None,
            empty_boundaries(),
            &[],
        );
        assert!(
            r.topology
                .external_processes
                .iter()
                .all(|p| p.covered_by.is_none()),
            "neither os.system nor subprocess.Popen is ever a match candidate"
        );
    }

    /// WS3: each hand-rolled pattern the scan sighted becomes ONE paired
    /// finding. The pairing is with the topology bucket: a wrappable target
    /// makes the finding actionable now (warn); an unreachable one is a
    /// once-wrapped lead (info). The poll finding names the `poll` primitive
    /// as the replacement (WS5 pairing).
    #[test]
    fn simplification_findings_pair_with_topology_buckets() {
        use crate::scan::{SimplificationSighting, TransportClass};
        let mut scan = ScanResult {
            files_scanned: 2,
            python_available: true,
            ..ScanResult::default()
        };
        // Wrappable target (tracked transport) with a hand-rolled retry.
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
        scan.simplifications.push(SimplificationSighting {
            file: "app.py".into(),
            line: 12,
            kind: "hand-rolled-retry".into(),
            function: "caller".into(),
            targets: vec!["api.ok.com".into()],
        });
        // Unreachable target (stdlib urllib) with a hand-rolled poll — the
        // claude-trader shape until WS4 flips urllib to tracked.
        scan.targets.insert(
            "api.tavily.com".into(),
            TargetEvidence {
                class: TargetClass::Host,
                sightings: [Sighting {
                    file: "fetch_short_metrics.py".into(),
                    line: 39,
                }]
                .into_iter()
                .collect(),
            },
        );
        scan.host_transports
            .insert("api.tavily.com".into(), TransportClass::UntrackedKnown);
        scan.simplifications.push(SimplificationSighting {
            file: "fetch_short_metrics.py".into(),
            line: 83,
            kind: "hand-rolled-poll".into(),
            function: "_poll_research".into(),
            targets: vec!["api.tavily.com".into()],
        });
        let r = build_report(
            &scan,
            &BTreeSet::new(),
            default_policy(),
            default_journal(),
            None,
            empty_boundaries(),
            &[],
        );
        let retry = r
            .findings
            .iter()
            .find(|f| f.topic == "hand-rolled-retry")
            .expect("retry finding");
        assert_eq!(retry.level, "warn", "wrappable target → actionable now");
        assert!(retry.detail.contains("api.ok.com"));
        assert!(retry.detail.contains("app.py:12"));
        assert!(retry.detail.contains("caller"));
        let poll = r
            .findings
            .iter()
            .find(|f| f.topic == "hand-rolled-poll")
            .expect("poll finding");
        assert_eq!(poll.level, "info", "unreachable target → once-wrapped lead");
        assert!(poll.detail.contains("fetch_short_metrics.py:83"));
        assert!(poll.action.contains("poll"), "names the poll primitive");
        // The WS2 closed follow-up vocabulary is NOT extended by WS3.
        assert!(
            r.follow_ups
                .iter()
                .all(|f| !f.code.starts_with("hand-rolled") && f.code != "silent-swallow"),
            "{:?}",
            r.follow_ups
        );
        // Simplification findings are honesty leads, never configuration errors.
        assert!(r.ok);
    }

    /// The ranked follow-up list (WS2): every honesty signal that needs a human/
    /// agent to chase becomes one entry in a closed vocabulary, sorted by
    /// (rank, code, subject) with rank = ascending Keel-confidence.
    #[test]
    fn follow_ups_are_ranked_closed_vocabulary_and_sorted() {
        use crate::scan::{DepAverseFile, SubprocessSighting, TransportClass};
        let mut scan = ScanResult {
            files_scanned: 4,
            python_available: true,
            ..ScanResult::default()
        };
        // Two unreachable hosts (rank 1) — inserted in reverse order to prove
        // the sort, not the insertion order, decides.
        for (host, file) in [("api.zeta.com", "z.py"), ("api.alpha.com", "a.py")] {
            scan.targets.insert(
                host.into(),
                TargetEvidence {
                    class: TargetClass::Host,
                    sightings: [Sighting {
                        file: file.into(),
                        line: 1,
                    }]
                    .into_iter()
                    .collect(),
                },
            );
            scan.host_transports
                .insert(host.into(), TransportClass::UntrackedKnown);
        }
        // One external process (rank 3).
        scan.subprocesses.push(SubprocessSighting {
            file: "launch.py".into(),
            line: 12,
            launcher: "subprocess.run".into(),
            command: "uvx alpaca-mcp-server".into(),
            argv: Some(vec!["uvx".into(), "alpaca-mcp-server".into()]),
        });
        // One excluded host (rank 4).
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
        scan.dependency_averse.push(DepAverseFile {
            file: "risk_gate.py".into(),
            reason: "stdlib-only + name/docstring signal: risk".into(),
        });
        // Pre-existing resilience alongside a wrapped lib (rank 5).
        scan.libs.insert("httpx".to_owned());
        scan.resilience_libs.insert("tenacity".to_owned());

        let r = build_report(
            &scan,
            &BTreeSet::new(),
            default_policy(),
            default_journal(),
            None,
            empty_boundaries(),
            &[],
        );

        let got: Vec<(u32, &str, &str)> = r
            .follow_ups
            .iter()
            .map(|f| (f.rank, f.code, f.subject.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                (1, "url-no-transport", "api.alpha.com"),
                (1, "url-no-transport", "api.zeta.com"),
                (
                    3,
                    "subprocess-blind-spot",
                    "1 externally-launched process(es)"
                ),
                (4, "dependency-averse-excluded", "api.broker.com"),
                (5, "preexisting-resilience", "tenacity"),
            ]
        );
        // Every detail is non-empty keel-authored text.
        assert!(r.follow_ups.iter().all(|f| !f.detail.is_empty()));
        // follow_ups never affect ok.
        assert!(r.ok);
        // The human view carries the section, ranked.
        let text = human(&r);
        assert!(text.contains("follow-ups"));
        assert!(text.contains("[url-no-transport] api.alpha.com"));
    }

    /// No signals → the field is present and empty (agents can rely on the key).
    #[test]
    fn no_signals_means_empty_follow_ups() {
        use crate::scan::TransportClass;
        let scan = scan_with("api.example.com", TargetClass::Host, &["httpx"]);
        // httpx is a tracked transport for this host.
        let mut scan = scan;
        scan.host_transports
            .insert("api.example.com".into(), TransportClass::Tracked);
        let r = build_report(
            &scan,
            &BTreeSet::new(),
            default_policy(),
            default_journal(),
            None,
            empty_boundaries(),
            &[],
        );
        assert!(r.follow_ups.is_empty(), "{:?}", r.follow_ups);
    }

    /// The override precedence documented on [`classify_topology`]: a
    /// wrapped-at-runtime target or an `llm:*` target is wrappable by
    /// construction, regardless of what the transport check or the
    /// dependency-averse check would otherwise conclude. Runtime evidence (or
    /// the LLM pack's own wrapping) beats static doubt.
    #[test]
    #[allow(clippy::too_many_lines)] // WS6 added a `build_report` arg; the fixture setup is
    // already the longest legitimate part of this test, not the new plumbing.
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
        let r = build_report(
            &scan,
            &wrapped,
            default_policy(),
            default_journal(),
            None,
            empty_boundaries(),
            &[],
        );

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
            cmd_match: BTreeMap::new(),
            fix: None,
        };
        let r = build_report(
            &scan,
            &BTreeSet::new(),
            policy,
            default_journal(),
            None,
            empty_boundaries(),
            &[],
        );

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

    /// Issue #17, end-to-end: a pure-Node project (no Python files at all) that
    /// imports `@modelcontextprotocol/sdk` must light up doctor's merged
    /// python+node `mcp` adapter row as `detected: true` — not be reported as an
    /// "invisible" unadapted library. Drives the real `scan()` over a temp dir
    /// so the whole JS-scan → cross-language `libs` merge → `build_report` path
    /// is exercised, reproducing the exact symptom the issue reported
    /// (`detected: false` for a Node-only MCP client). The `agent_pack_*` test
    /// above pins the doctor half from a synthetic `libs`; this pins the wiring
    /// from a real filesystem scan with zero Python.
    #[test]
    fn pure_node_mcp_project_lights_up_the_mcp_adapter() {
        use std::fs;
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("server.ts"),
            "import { Client } from \"@modelcontextprotocol/sdk/client/index.js\";\n",
        )
        .unwrap();
        let scan = scan::scan(dir.path());
        assert!(
            scan.libs.contains("mcp"),
            "a Node-only scan must carry `mcp` into ScanResult.libs: {:?}",
            scan.libs
        );

        let r = build_report(
            &scan,
            &BTreeSet::new(),
            default_policy(),
            default_journal(),
            None,
            empty_boundaries(),
            &[],
        );
        let mcp = r
            .adapters
            .iter()
            .find(|a| a.lib == "mcp")
            .expect("mcp REGISTRY row");
        assert!(
            mcp.detected,
            "pure-Node MCP project must show the mcp adapter detected"
        );
        assert_eq!(mcp.target, "mcp:<server>");
        assert!(
            !r.coverage.invisible.iter().any(|l| l == "mcp"),
            "a registered adapter is coverage, not an invisible finding: {:?}",
            r.coverage.invisible
        );
    }

    fn default_policy() -> PolicyValidation {
        PolicyValidation {
            check: PolicyCheck {
                field: None,
                message: None,
                present: false,
                valid: true,
            },
            cmd_match: BTreeMap::new(),
            fix: None,
        }
    }

    /// WS4: the stdlib urllib.request pack has a REGISTRY row (keyed to the
    /// Python runtime version — the documented convention exception), a
    /// scanned `urllib.request` import counts as detected/not-invisible, and
    /// its hosts are wrappable, not unreachable.
    #[test]
    fn urllib_request_is_a_registered_tracked_adapter() {
        let mut scan = scan_with("api.tavily.com", TargetClass::Host, &["urllib.request"]);
        scan.host_transports
            .insert("api.tavily.com".into(), TransportClass::Tracked);
        let r = build_report(
            &scan,
            &BTreeSet::new(),
            default_policy(),
            default_journal(),
            None,
            empty_boundaries(),
            &[],
        );
        let row = r
            .adapters
            .iter()
            .find(|a| a.lib == "urllib.request")
            .expect("REGISTRY row for urllib.request");
        assert!(row.detected);
        assert_eq!(row.status, "pinned");
        assert_eq!(row.target, "host");
        assert!(
            r.coverage.invisible.is_empty(),
            "{:?}",
            r.coverage.invisible
        );
        assert!(r.topology.wrappable.contains(&"api.tavily.com".to_owned()));
        assert!(
            r.topology.unreachable.is_empty(),
            "{:?}",
            r.topology.unreachable
        );
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
            empty_boundaries(),
            &[],
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
            empty_boundaries(),
            &[],
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
            empty_boundaries(),
            &[],
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
            cmd_match: BTreeMap::new(),
            fix: None,
        };
        let r = build_report(
            &scan,
            &wrapped,
            policy,
            default_journal(),
            None,
            empty_boundaries(),
            &[],
        );
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
            cmd_match: BTreeMap::new(),
            fix: None,
        };
        let journal = JournalReport {
            backend: "postgres",
            location: "postgres://\u{2026}@db.internal/keel".to_owned(),
            source: "keel.toml",
            supported: false,
        };
        let r = build_report(
            &scan,
            &wrapped,
            policy,
            journal,
            None,
            empty_boundaries(),
            &[],
        );
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

    /// Whether `python3` is on PATH — gates the Python-scan end-to-end test
    /// below, mirroring `scan::python`'s test helper (private to that module,
    /// so duplicated here rather than shared across crates).
    fn python3_present() -> bool {
        std::process::Command::new("python3")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    /// WS6: a resumable flow recorded under a different code hash surfaces as
    /// the rank-6 `code-hash-stale` follow-up.
    #[test]
    fn code_hash_stale_flow_emits_the_rank6_follow_up() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let schema = std::fs::read_to_string(root.join("contracts/journal.sql")).unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let keel = dir.path().join(".keel");
        std::fs::create_dir_all(&keel).unwrap();
        let conn = rusqlite::Connection::open(keel.join("journal.db")).unwrap();
        conn.execute_batch(&schema).unwrap();
        let t0: i64 = 1_783_728_000_000;
        conn.execute(
            "INSERT INTO flows (flow_id, entrypoint, args_hash, code_hash, status, created_at, \
             updated_at) VALUES ('01STALEFLOW', 'py:pipeline.ingest:main', 'ah-1', \
             'deadbeefdeadbeef', 'running', ?1, ?1)",
            rusqlite::params![t0],
        )
        .unwrap();
        // A script on disk whose hash will never match the synthetic recorded
        // value above.
        let script_dir = dir.path().join("pipeline");
        std::fs::create_dir_all(&script_dir).unwrap();
        std::fs::write(script_dir.join("ingest.py"), "def main():\n    pass\n").unwrap();

        let r = run(dir.path());
        let f = r.json["follow_ups"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["code"] == "code-hash-stale")
            .expect("code-hash-stale emitted");
        assert_eq!(f["rank"], 6);
        assert!(f["detail"].as_str().unwrap().contains("keel replay"));
    }

    /// WS2 hardening: doctor and init --diff (the two MCP-exposed report
    /// producers) must never emit raw source content. Allowed interpolations
    /// are ONLY: hostnames, file paths, lib names, literal subprocess argv, and
    /// keel-authored sentences. Canary strings placed in every other syntactic
    /// position must not survive into either the JSON or the human rendering.
    #[test]
    fn doctor_and_init_diff_never_leak_raw_source() {
        const CANARIES: [&str; 5] = [
            "CANARY_COMMENT_9f31",
            "CANARY_SECRET_9f31",
            "CANARY_QUERY_9f31",
            "CANARY_DOCSTRING_9f31",
            "CANARY_QUERY2_9f31",
        ];
        if !python3_present() {
            eprintln!("skip: python3 not available");
            return;
        }
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("app.py"),
            r#"import time
import urllib.request
import httpx
import tenacity
# CANARY_COMMENT_9f31 must never appear in any report
TOKEN = "CANARY_SECRET_9f31"
U = "https://api.leak.example/v1?key=CANARY_QUERY_9f31"

def caller():
    attempt = 0
    while True:
        try:
            return httpx.get(U)
        except Exception:
            # CANARY_COMMENT_9f31 must never appear in any report
            # Deliberate handler-local canary: an unused local-variable
            # assignment RHS, a syntactic position distinct from the other
            # four canaries above (comment, module const, URL query param,
            # docstring) — not dead code left behind by mistake.
            local_secret = "CANARY_QUERY2_9f31"
            attempt += 1
            time.sleep(1)
"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("risk_gate.py"),
            "\"\"\"risk gate. CANARY_DOCSTRING_9f31 stdlib only.\"\"\"\n\
             import json\n\
             G = \"https://api.gateonly.example/v2?tok=CANARY_QUERY2_9f31\"\n",
        )
        .unwrap();
        let doctor = run(dir.path());
        let doctor_json = crate::render::json_string(&doctor.json);
        let init = crate::init::run(
            dir.path(),
            crate::init::InitOptions {
                diff: true,
                stamp: false,
                agents: false,
            },
        );
        let init_json = crate::render::json_string(&init.json);
        for canary in CANARIES {
            assert!(!doctor_json.contains(canary), "doctor json leaks {canary}");
            assert!(
                !doctor.human.contains(canary),
                "doctor human leaks {canary}"
            );
            assert!(
                !init_json.contains(canary),
                "init --diff json leaks {canary}"
            );
            assert!(
                !init.human.contains(canary),
                "init --diff human leaks {canary}"
            );
        }
        // Sanity: the report DID see the project (hosts present) — the canaries
        // are absent because of scoping, not because the scan saw nothing.
        assert!(doctor_json.contains("api.leak.example"));
        // Sanity: the fixture's hand-rolled retry loop (Task 3.3's `--diff`
        // notes path) actually fired — proving the canary-absence assertions
        // above exercised the new note-rendering code, not an empty notes
        // list that would trivially satisfy them.
        assert!(
            init_json.contains("hand-rolled-retry"),
            "init --diff notes should surface the hand-rolled retry loop: {init_json}"
        );
    }

    // ---- boundaries ----

    /// A `Boundaries` frame for a project root with no governance files — what
    /// every `build_report` unit test wants unless it is specifically testing
    /// governance detection. Uses the real constructor so the tests cannot
    /// drift from `run`'s behavior.
    fn empty_boundaries() -> Boundaries {
        let dir = tempfile::TempDir::new().unwrap();
        boundaries(dir.path())
    }

    #[test]
    fn report_always_carries_boundaries() {
        let scan = ScanResult::default();
        let wrapped = BTreeSet::new();
        let r = build_report(
            &scan,
            &wrapped,
            default_policy(),
            default_journal(),
            None,
            empty_boundaries(),
            &[],
        );
        assert!(r.boundaries.parsed_languages.contains(&"js-ts"));
        assert!(r.boundaries.unparsed.contains(&"ci-workflow"));
        // Boundaries are a frame, not work: they must never inflate findings.
        assert!(!r.findings.iter().any(|f| f.topic == "evaluation-protocol"));
        assert!(!r.findings.iter().any(|f| f.topic == "governance-boundary"));
    }

    #[test]
    fn boundaries_list_governance_files_that_exist() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "# rules\n").unwrap();
        let b = boundaries(dir.path());
        assert_eq!(b.governance_files, vec!["CLAUDE.md"]);

        std::fs::write(dir.path().join("AGENTS.md"), "# keel\n").unwrap();
        let b = boundaries(dir.path());
        assert_eq!(b.governance_files, vec!["CLAUDE.md", "AGENTS.md"]);
    }

    #[test]
    fn boundaries_are_empty_but_present_without_governance_files() {
        let dir = tempfile::TempDir::new().unwrap();
        let b = boundaries(dir.path());
        assert!(b.governance_files.is_empty());
        // The standing facts are unconditional — an agent that reached the tool
        // without the skill must always learn what was not parsed.
        assert!(b.parsed_languages.contains(&"python"));
        assert!(b.unparsed.contains(&"shell"));
        assert!(b.protocol.contains("Baseline"));
    }

    #[test]
    fn human_report_carries_a_compact_boundaries_section() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "# rules\n").unwrap();
        let scan = ScanResult::default();
        let r = build_report(
            &scan,
            &BTreeSet::new(),
            default_policy(),
            default_journal(),
            None,
            boundaries(dir.path()),
            &[],
        );
        let text = human(&r);
        assert!(text.contains("\nboundaries\n"), "{text}");
        assert!(text.contains("python, js-ts"), "{text}");
        assert!(text.contains("CLAUDE.md"), "{text}");
        // Compact: the whole section, not one line per fact.
        let section = text.split("\nboundaries\n").nth(1).unwrap();
        let lines = section.lines().take_while(|l| l.starts_with("  ")).count();
        assert!(
            lines <= 3,
            "boundaries section is {lines} lines:\n{section}"
        );
    }

    // ---- orchestration blind spot ----

    #[test]
    fn orchestration_sightings_become_a_finding() {
        let mut scan = ScanResult::default();
        for (file, line) in [
            ("scripts/run_autonomous.sh", 3),
            ("scripts/run_autonomous.sh", 9),
        ] {
            scan.orchestration.push(scan::OrchestrationSighting {
                file: file.to_owned(),
                line,
                kind: "lockfile-mutex".to_owned(),
                snippet: "flock -n /tmp/x.lock || exit 0".to_owned(),
            });
        }
        let r = build_report(
            &scan,
            &BTreeSet::new(),
            default_policy(),
            default_journal(),
            None,
            empty_boundaries(),
            &[],
        );
        let f = r
            .findings
            .iter()
            .find(|f| f.topic == "orchestration-blind-spot")
            .expect("orchestration finding present");
        assert_eq!(f.level, "warn");
        assert!(f.detail.contains("run_autonomous.sh"), "{}", f.detail);
        // Two sightings in one file name it once.
        assert_eq!(
            f.detail.matches("run_autonomous.sh").count(),
            1,
            "{}",
            f.detail
        );
    }

    /// A monorepo must not get a multi-kilobyte finding.
    #[test]
    fn orchestration_finding_caps_the_file_list() {
        let mut scan = ScanResult::default();
        for i in 0..40 {
            scan.orchestration.push(scan::OrchestrationSighting {
                file: format!("scripts/s{i:02}.sh"),
                line: 1,
                kind: "pid-check".to_owned(),
                snippet: "kill -0 $PID".to_owned(),
            });
        }
        scan.orchestration.sort();
        let r = build_report(
            &scan,
            &BTreeSet::new(),
            default_policy(),
            default_journal(),
            None,
            empty_boundaries(),
            &[],
        );
        let f = r
            .findings
            .iter()
            .find(|f| f.topic == "orchestration-blind-spot")
            .unwrap();
        assert!(f.detail.contains("and 35 more"), "{}", f.detail);
        assert!(f.detail.len() < 600, "detail is {} bytes", f.detail.len());
    }

    #[test]
    fn no_orchestration_no_finding() {
        let scan = ScanResult::default();
        let r = build_report(
            &scan,
            &BTreeSet::new(),
            default_policy(),
            default_journal(),
            None,
            empty_boundaries(),
            &[],
        );
        assert!(
            !r.findings
                .iter()
                .any(|f| f.topic == "orchestration-blind-spot")
        );
    }

    #[test]
    fn orchestration_sightings_become_a_ranked_follow_up() {
        let mut scan = ScanResult::default();
        scan.orchestration.push(scan::OrchestrationSighting {
            file: "scripts/run_autonomous.sh".to_owned(),
            line: 3,
            kind: "lockfile-mutex".to_owned(),
            snippet: "flock -n /tmp/x.lock || exit 0".to_owned(),
        });
        let r = build_report(
            &scan,
            &BTreeSet::new(),
            default_policy(),
            default_journal(),
            None,
            empty_boundaries(),
            &[],
        );
        let up = r
            .follow_ups
            .iter()
            .find(|f| f.code == "orchestration-blind-spot")
            .expect("orchestration follow-up present");
        assert_eq!(up.rank, 2);
        assert!(!up.detail.is_empty());
        // follow_ups never affect ok.
        assert!(r.ok);
    }
}
