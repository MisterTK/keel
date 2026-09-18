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
use std::path::{Path, PathBuf};

use keel_core_api::policy::{FlowMatchRule, Policy};
use keel_journal::Activation;
use serde::Serialize;

use crate::cmd_match::{compile_cmd_rules, match_argv};
use crate::diff::{PolicyOp, PolicyPath, Proposal, propose, resolve_dotted_path};
use crate::render::to_json;
use crate::scan::{ScanResult, SimplificationSighting, TransportClass};
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
    /// The policy file this report read, project-relative (`"keel.toml"`), or
    /// `None` when there is none. Machine-checkable: a consumer comparing two
    /// environments can see *which* file — if any — each one actually loaded,
    /// rather than inferring it from `present`.
    path: Option<String>,
    present: bool,
    valid: bool,
}

/// One actionable finding. Where the finding implies a policy edit, `fix`
/// carries the applyable form (dx-spec §5, diffs as the lingua franca): a
/// unified `patch` for `git apply` plus structured `changes`.
///
/// `fix_ref` is the structured half of "this finding's fix lives elsewhere":
/// the `file:line` of the sighting whose finding holds the patch, for the case
/// where two findings would propose the SAME edit and only one patch can apply
/// (#107). Absent — and omitted from the JSON entirely — on every finding that
/// either carries its own `fix` or needs none, so existing consumers see the
/// exact bytes they saw before.
#[derive(Debug, Serialize)]
struct Finding {
    action: String,
    detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    fix: Option<Proposal>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fix_ref: Option<String>,
    level: &'static str,
    topic: &'static str,
}

/// One ranked follow-up: a lead Keel cannot chase itself, phrased for the
/// agent/human reading the report to work top-down. `code` is a CLOSED set —
/// url-no-transport | orchestration-blind-spot | subprocess-blind-spot |
/// dependency-averse-excluded | local-host-excluded | reserved-name-excluded |
/// test-only-excluded | preexisting-resilience | sdk-client-timeout |
/// code-hash-stale — ranked lowest-Keel-confidence first
/// (rank 1 = Keel knows least, investigate first). Text is entirely
/// keel-authored; only hostnames, file paths, and lib names are interpolated.
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
/// reason why — see [`Topology`]. `kind` is a short, stable category tag
/// (`"local/loopback"`, `"dependency-averse"`, `"untracked-transport"`,
/// `"unknown-transport"`) a renderer can label the entry with directly,
/// rather than parsing `reason` prose to guess the category (#64 — a
/// generic hardcoded label was actively wrong once a second `excluded`
/// category existed). `pub(crate)`: `init.rs` reuses [`classify_topology`]
/// to skip proposals for excluded hosts and print why.
#[derive(Debug, Serialize)]
pub(crate) struct TopologyEntry {
    pub(crate) host: String,
    pub(crate) kind: &'static str,
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
    /// Whether the sighting's file is test code ([`scan::is_test_path`]) — a
    /// test-only launch is counted separately from the production blind spots
    /// rather than warned about (WS5).
    pub(crate) in_tests: bool,
    pub(crate) launcher: String,
    pub(crate) line: u32,
    /// `"python"` / `"node"` / `None` — see [`scan::SubprocessSighting::child_runtime`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) child_runtime: Option<String>,
    /// `"python-pth"` | `"python-pth-if-env-passed"` | `"node-needs-NODE_OPTIONS"`
    /// | `None` — see [`inherits_activation`]. Only `Some("python-pth")` moves
    /// a sighting out of the blind-spot list (issue #91).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) inherits_activation: Option<&'static str>,
}

/// Whether the child launched by this sighting inherits Keel's activation:
/// `"python-pth"` (a Python child whose environment is inherited — the
/// keelrun `.pth` self-activates it when `KEEL_ENABLE` reaches it),
/// `"python-pth-if-env-passed"` (a Python child whose env kwarg could not be
/// statically classified — it self-activates only if the caller happens to
/// pass `KEEL_ENABLE` through), `"node-needs-NODE_OPTIONS"` (a Node child —
/// activation needs `NODE_OPTIONS="--import keelrun/register"`, which an
/// inherited/unknown env alone does not supply), or `None` (an env the
/// scanner classified as `"replaced"`, or a launcher that is not a
/// recognizable Python/Node runtime at all). Only the first case is honest
/// to call covered-when-active (issue #91).
fn inherits_activation(s: &scan::SubprocessSighting) -> Option<&'static str> {
    match (s.child_runtime.as_deref(), s.env_inheritance.as_str()) {
        (Some("python"), "inherited") => Some("python-pth"),
        (Some("python"), "unknown") => Some("python-pth-if-env-passed"),
        (Some("node"), "inherited" | "unknown") => Some("node-needs-NODE_OPTIONS"),
        _ => None,
    }
}

/// Render one blind-spot-list entry, annotating the two cases that still
/// self-activate under some condition (issue #91) so the warning is honest
/// about how close each one is to being covered.
fn format_process_entry(p: &ExternalProcess) -> String {
    let annotation = match p.inherits_activation {
        Some("python-pth-if-env-passed") => {
            "; python child, env=unknown — activates only if KEEL_ENABLE is passed"
        }
        Some("node-needs-NODE_OPTIONS") => {
            "; node child — needs NODE_OPTIONS=\"--import keelrun/register\""
        }
        _ => "",
    };
    format!(
        "`{}` ({} at {}:{}{annotation})",
        p.command, p.launcher, p.file, p.line
    )
}

/// The three-bucket honesty topology (dx-spec §2): every host Keel's static
/// scan saw, sorted into exactly one of "wrap it" (a tracked transport is in
/// reach, or the target is wrapped-at-runtime/`llm:*` by construction),
/// "can't reach it" (no adapted transport in reach — Keel is blind here
/// regardless of policy), or "shouldn't reach it" (a local/loopback host, an
/// RFC 2606/5737 reserved name, a host sighted only in test files, or one
/// sighted only inside a file the scan judged dependency-averse — all
/// excluded from proposed policy on purpose). `external_processes` is the
/// adjacent, host-independent honesty signal: traffic inside an
/// externally-launched process Keel cannot see at all, no matter which bucket
/// its host would otherwise land in.
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
    /// File classes read coarsely by directive, not parsed into an AST — so a
    /// verdict drawn from them is a lead about the packaging boundary, never a
    /// build. Today: the root build files' `COPY`/`ADD` lines (WS3).
    parsed_files: &'static [&'static str],
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
    /// `"verified"` when some recorded activation row matches this project's
    /// resolved policy identity, `"unverified"` otherwise — including when
    /// there is no discovery evidence at all (#92).
    runtime_activation: &'static str,
    /// Which runtime backend the verifying activation row named — `"native"`
    /// or `"stub"` (#129, the field `runtime_activation` explains: `keel
    /// doctor` previously had no way to say which backend actually ran,
    /// though the CLI's own live banner/JSON summary always could). `None`
    /// when `runtime_activation` is `"unverified"`, or when the verifying row
    /// predates #129 and so was written with no `backend` column at all.
    activation_backend: Option<String>,
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
    /// Every declared `timeout` over `LRO_TIMEOUT_MS` (issue #80), as
    /// (subject path, timeout ms) — empty when `keel.toml` is absent,
    /// invalid, or simply declares no LRO-sized timeout. Subject paths are
    /// already deterministically ordered (`defaults.outbound`,
    /// `defaults.llm`, then `policy.target`'s `BTreeMap` iteration order).
    lro_timeouts: Vec<(String, u64)>,
    fix: Option<Proposal>,
    /// The raw `keel.toml` text this validation read — the base document a
    /// proposal edits (`None` when the file is absent or unreadable, which
    /// is distinct from `Some("")`: only the former selects the `/dev/null`
    /// creation header).
    text: Option<String>,
    /// `[flows] entrypoints` non-empty or `[flows.match]` non-empty (issue
    /// #90) — whether this project has anything durable for an ephemeral
    /// journal to lose. `false` when `keel.toml` is absent or invalid.
    flows_configured: bool,
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
    let activations = match evidence::read_activations(project) {
        Ok(a) => a,
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
    let config_above_cwd_finding = config_above_cwd_finding(project);
    let boundaries = boundaries(project);
    let build_files = crate::dockerfile::analyze(project);
    let artifacts = deploy_artifacts(project, &build_files);
    let stale_flows = crate::flows::stale_code_hash_flows(project);
    let (runtime_activation_value, activation_backend_value) =
        runtime_activation(project, policy.check.present, &activations);
    let report = build_report(
        &scan,
        &discovery,
        policy,
        journal,
        agents_cli_finding,
        config_above_cwd_finding,
        boundaries,
        &build_files,
        &artifacts,
        &stale_flows,
        runtime_activation_value,
        activation_backend_value,
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
        fix_ref: None,
        level: "warn",
        topic: "preexisting-resilience",
    })
}

/// The three honesty findings that carry [`Topology`]'s buckets into the
/// findings list: one `url-no-transport` warning per unreachable host, one
/// `subprocess-blind-spot` warning naming every externally-launched process
/// (if any), and one info per excluded host — topic and action keyed off
/// [`TopologyEntry::kind`](TopologyEntry) so a loopback exclusion (#64)
/// never gets the dependency-averse-specific `# keel: include` advice, which
/// does not apply to it. None of these are configuration errors — they
/// never affect `ok`.
#[allow(clippy::too_many_lines)] // one straight-line section per finding topic;
// WS5 added the test-only subprocess section, not new branching depth.
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
            fix_ref: None,
            level: "warn",
            topic: "url-no-transport",
        });
    }
    let (covered, unmatched): (Vec<_>, Vec<_>) = topology
        .external_processes
        .iter()
        .partition(|p| p.covered_by.is_some());
    // WS5: a launch that only ever happens from test code is not a production
    // blind spot. It is still reported — dropping evidence silently would be
    // its own honesty violation — but as `info`, out of the `warn` list.
    let (in_tests, unmatched): (Vec<_>, Vec<_>) = unmatched.into_iter().partition(|p| p.in_tests);
    // Issue #91: a Python child that inherits our environment self-activates
    // via the keelrun `.pth` when active — the one case Keel can honestly
    // call covered-when-active, so it moves out of the blind-spot list into
    // its own `info` finding rather than being warned about.
    let (inheriting, uncovered): (Vec<_>, Vec<_>) = unmatched
        .into_iter()
        .partition(|p| p.inherits_activation == Some("python-pth"));
    if !uncovered.is_empty() {
        let cmds: Vec<String> = uncovered
            .iter()
            .copied()
            .map(format_process_entry)
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
            fix_ref: None,
            level: "warn",
            topic: "subprocess-blind-spot",
        });
    }
    if !inheriting.is_empty() {
        let cmds: Vec<String> = inheriting
            .iter()
            .map(|p| format!("`{}` ({} at {}:{})", p.command, p.launcher, p.file, p.line))
            .collect();
        findings.push(Finding {
            action: "Confirm keelrun is installed in the child's interpreter and that the env \
                      you pass keeps KEEL_ENABLE (and KEEL_CWD if set); `KEEL_LOG_FORMAT=json` \
                      in the child makes its activation line greppable."
                .to_owned(),
            detail: format!(
                "{} externally-launched Python process(es) inherit this process's environment \
                 and self-activates via the keelrun .pth when KEEL_ENABLE reaches them: {}.",
                inheriting.len(),
                cmds.join(", ")
            ),
            fix: None,
            fix_ref: None,
            level: "info",
            topic: "subprocess-blind-spot",
        });
    }
    if !in_tests.is_empty() {
        let cmds: Vec<String> = in_tests
            .iter()
            .map(|p| format!("`{}` ({} at {}:{})", p.command, p.launcher, p.file, p.line))
            .collect();
        findings.push(Finding {
            action: "No action needed for test-only launches.".to_owned(),
            detail: format!(
                "{} externally-launched process(es) in test files only: {}.",
                in_tests.len(),
                cmds.join(", ")
            ),
            fix: None,
            fix_ref: None,
            level: "info",
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
            fix_ref: None,
            level: "info",
            topic: "subprocess-blind-spot",
        });
    }
    for entry in &topology.excluded {
        let action = match entry.kind {
            "local/loopback" => {
                "Confirm this is a local/test target, not a real dependency; \
                                  run under keel to gather runtime evidence, or add it to \
                                  keel.toml explicitly."
            }
            "reserved-name" => {
                "Nothing to do — a reserved/documentation name is a fixture by definition."
            }
            "test-only" => {
                "Nothing to do unless production code also reaches this host; if it does, \
                 run under keel so runtime evidence promotes it."
            }
            _ => {
                "Confirm the exclusion is intended; add `# keel: include` to the file to \
                  override."
            }
        };
        findings.push(Finding {
            action: action.to_owned(),
            detail: format!("`{}` — {}.", entry.host, entry.reason),
            fix: None,
            fix_ref: None,
            level: "info",
            topic: excluded_kind_topic(entry.kind),
        });
    }
    findings
}

/// The finding `topic` / follow-up `code` for a
/// [`TopologyEntry::kind`](TopologyEntry) landing in `topology.excluded`
/// (#64) — shared between [`topology_findings`] and [`build_follow_ups`] so
/// the two surfaces never disagree about an excluded host's category slug.
/// `"dependency-averse"` is the pre-#64 default: any kind other than the
/// loopback one (#64) and the two WS5 fixture kinds added here falls back to
/// it, with a debug-build assertion (not a release panic) catching genuine
/// `classify_topology` drift.
fn excluded_kind_topic(kind: &str) -> &'static str {
    match kind {
        "local/loopback" => "local-host-excluded",
        "reserved-name" => "reserved-name-excluded",
        "test-only" => "test-only-excluded",
        other => {
            debug_assert!(
                other == "dependency-averse",
                "classify_topology emitted an unknown excluded kind: {other}"
            );
            "dependency-averse-excluded"
        }
    }
}

/// One route-key `poll` block doctor proposes for an SDK poll shape (spec
/// §3.4 table). `interval`/`deadline` are proposal defaults an operator tunes.
struct RouteKeyProposal {
    key: &'static str,
    field: &'static str,
    terminal: &'static str,
    /// `Some("pending")` when this route's terminal field is OMITTED (not
    /// `false`) while the job is still running — i.e. `until.absent =
    /// "pending"` (CCR-11) belongs in the emitted block. `None` keeps the
    /// schema default (`fail_open`) for routes where absence is not known to
    /// mean pending; this is a per-proposal field rather than a hardcoded
    /// addition to [`render_route_block`]'s format string specifically so a
    /// future non-Google proposal doesn't silently inherit "absence means
    /// pending" — that inference must be made per API family, not per
    /// template.
    absent: Option<&'static str>,
    /// Which Google surface this route belongs to, or `None` for a proposal
    /// that is not surface-scoped (every OpenAI and Anthropic route). A
    /// surface-scoped proposal is dropped when the project is known to use the
    /// other surface; `None` is never filtered.
    surface: Option<crate::surface::Surface>,
    /// Set by [`route_key_proposals_for`], which is the only thing that knows
    /// whether the surface is known well enough to drop the hedge.
    note: String,
}

/// The two Google notes, each with ONE home. [`route_key_proposals_for`]
/// appends its hedge suffix rather than restating the phrase, so a wording
/// edit here cannot be silently overwritten downstream.
const VERTEX_NOTE: &str = "Vertex AI operation read";
const GEMINI_NOTE: &str = "Gemini API operation read";
/// Appended to [`VERTEX_NOTE`] / [`GEMINI_NOTE`] only when the surface is
/// genuinely unknown and doctor has to propose both blocks.
const VERTEX_HEDGE: &str = " (delete if you use the Gemini API)";
const GEMINI_HEDGE: &str = " (delete if you use Vertex AI)";

/// The route keys that carry an operation read for one `(target, SDK poll
/// shape)` pair. Unknown pairs propose nothing — doctor never guesses a route.
fn route_key_proposals(target: &str, sdk_polls: &[String]) -> Vec<RouteKeyProposal> {
    let mut out = Vec::new();
    for shape in sdk_polls {
        match (target, shape.as_str()) {
            ("llm:google-genai", "operations.get") => {
                out.push(RouteKeyProposal {
                    key: "POST *-aiplatform.googleapis.com/*:fetchPredictOperation",
                    field: "done",
                    terminal: "[true]",
                    // A running google.longrunning.Operation omits `done`
                    // entirely (proto3 JSON drops a false bool) — absence IS
                    // the pending signal here, not an unknown shape (#128,
                    // CCR-11).
                    absent: Some("pending"),
                    surface: Some(crate::surface::Surface::Vertex),
                    note: VERTEX_NOTE.to_owned(),
                });
                out.push(RouteKeyProposal {
                    key: "GET generativelanguage.googleapis.com/*/operations/*",
                    field: "done",
                    terminal: "[true]",
                    absent: Some("pending"),
                    surface: Some(crate::surface::Surface::GeminiApi),
                    note: GEMINI_NOTE.to_owned(),
                });
            }
            ("llm:openai", "batches.retrieve") => out.push(RouteKeyProposal {
                key: "GET api.openai.com/v1/batches/*",
                field: "status",
                terminal: "[\"completed\", \"failed\", \"expired\", \"cancelled\"]",
                // OpenAI's batch/video/job status bodies always carry
                // `status`; an absent field here is genuinely unknown shape,
                // so the schema default (fail_open) stays.
                absent: None,
                surface: None,
                note: "OpenAI batch status".to_owned(),
            }),
            ("llm:openai", "videos.retrieve") => out.push(RouteKeyProposal {
                key: "GET api.openai.com/v1/videos/*",
                field: "status",
                terminal: "[\"completed\", \"failed\"]",
                absent: None,
                surface: None,
                note: "OpenAI video status".to_owned(),
            }),
            ("llm:openai", "fine_tuning.jobs.retrieve") => out.push(RouteKeyProposal {
                key: "GET api.openai.com/v1/fine_tuning/jobs/*",
                field: "status",
                terminal: "[\"succeeded\", \"failed\", \"cancelled\"]",
                absent: None,
                surface: None,
                note: "OpenAI fine-tuning job status".to_owned(),
            }),
            ("llm:anthropic", "batches.retrieve") => out.push(RouteKeyProposal {
                key: "GET api.anthropic.com/v1/messages/batches/*",
                field: "processing_status",
                terminal: "[\"ended\"]",
                absent: None,
                surface: None,
                note: "Anthropic message batch status".to_owned(),
            }),
            _ => {}
        }
    }
    out
}

/// [`route_key_proposals`], narrowed to the surfaces this project actually
/// uses, with the note reworded to match.
///
/// A known single surface yields one block and nothing to delete; `{both}` is
/// a fact rather than a hedge and wants both blocks kept; only a genuinely
/// unknown surface gets the "delete the one you do not use" hedge. See the
/// spec's §4.1.
fn route_key_proposals_for(
    target: &str,
    sdk_polls: &[String],
    detected: &[crate::surface::Surface],
) -> Vec<RouteKeyProposal> {
    use crate::surface::Surface;
    let mut out = route_key_proposals(target, sdk_polls);
    if detected.is_empty() {
        // Only a genuinely unknown surface gets the hedge appended; the table's
        // note is the plain phrase, and this is the one place the suffix lives.
        for p in &mut out {
            match p.surface {
                Some(Surface::Vertex) => p.note.push_str(VERTEX_HEDGE),
                Some(Surface::GeminiApi) => p.note.push_str(GEMINI_HEDGE),
                None => {}
            }
        }
    } else {
        out.retain(|p| p.surface.is_none_or(|s| detected.contains(&s)));
    }
    out
}

/// The cadence ONE route-key patch proposes, folded over every sighting that
/// shares that key set (#107). One patch governs a route all of those loops
/// travel, so each column is the most conservative value across them — the
/// MAXIMUM — and drops to the block's documented default the moment any one
/// member's value is unknown. Speeding a loop up is the failure mode that
/// matters here (an LRO poll storm); slowing one down is not.
#[derive(Debug, Clone, Copy, Default)]
struct RouteCadence {
    /// Max `interval_s` across the set, or `None` if any member has none.
    interval_s: Option<u32>,
    /// Max `deadline_s` across the set, or `None` if any member has none.
    deadline_s: Option<u32>,
    /// How many sightings share this key set (>= 1), for the provenance
    /// comment.
    sightings: usize,
}

impl RouteCadence {
    fn of(s: &SimplificationSighting) -> Self {
        Self {
            interval_s: s.interval_s,
            deadline_s: s.deadline_s,
            sightings: 1,
        }
    }

    fn fold(&mut self, s: &SimplificationSighting) {
        // `zip` is the whole rule: `None` on either side erases the column.
        self.interval_s = self.interval_s.zip(s.interval_s).map(|(a, b)| a.max(b));
        self.deadline_s = self.deadline_s.zip(s.deadline_s).map(|(a, b)| a.max(b));
        self.sightings += 1;
    }
}

/// The TOML block for one proposal, carrying its own evidence comment — the
/// no-raw-source rule holds: only the file, line, function name and a count
/// are interpolated.
///
/// `interval`/`deadline` come from [`RouteCadence`], i.e. the slowest values
/// observed across every loop on this route, defaulting to `10s` / `30m` per
/// column. Whenever more than one loop is involved the comment says which is
/// which, PER COLUMN: a defaulted column was not observed anywhere, and
/// claiming it was "the slowest of them" would be a false statement sitting
/// right beside the number.
fn render_route_block(
    p: &RouteKeyProposal,
    s: &SimplificationSighting,
    cadence: RouteCadence,
) -> String {
    let interval = cadence
        .interval_s
        .map_or_else(|| "10s".to_owned(), |v| format!("{v}s"));
    let deadline = cadence
        .deadline_s
        .map_or_else(|| "30m".to_owned(), |v| format!("{v}s"));
    let provenance = if cadence.sightings > 1 {
        // Only claim "slowest observed" for a column actually derived from the
        // loops; a defaulted column says so, and says why.
        let source = match (cadence.interval_s, cadence.deadline_s) {
            (Some(_), Some(_)) => "interval and deadline are the slowest of them",
            (Some(_), None) => {
                "interval is the slowest of them, deadline is Keel's default \
                 (a loop on this route declares none)"
            }
            (None, Some(_)) => {
                "deadline is the slowest of them, interval is Keel's default \
                 (a loop on this route declares none)"
            }
            (None, None) => {
                "interval and deadline are Keel's defaults \
                 (a loop on this route declares neither)"
            }
        };
        format!(
            "replaces {} hand-rolled polls on this route, the first in {}:{} ({}); {}",
            cadence.sightings, s.file, s.line, s.function, source
        )
    } else {
        format!(
            "replaces the hand-rolled poll in {}:{} ({})",
            s.file, s.line, s.function
        )
    };
    // A proposal carrying `absent` also carries its own upgrade caveat, on
    // the line above the key it annotates. `absent` is CCR-11; a Keel that
    // predates it rejects any unknown key under `until` with KEEL-E001, and
    // under `.pth`/`--import` auto-activation that KEEL-E001 puts Keel fully
    // OFF for that process. Applying an applyable patch must not be how an
    // operator with a mixed fleet discovers that. A standalone `#` line is
    // valid TOML and does not touch what the patch configures, so the
    // applyable output stays applyable. Deliberately carries no version
    // number: this binary is by construction new enough, `CARGO_PKG_VERSION`
    // would be stale in every built-but-unreleased tree, and a wrong floor
    // is worse than a named hazard with no floor.
    let (caveat, until) = p.absent.map_or_else(
        || {
            (
                String::new(),
                format!("{{ field = \"{}\", terminal = {} }}", p.field, p.terminal),
            )
        },
        |absent| {
            (
                "# `absent` is CCR-11: an older Keel rejects the key (KEEL-E001) and \
                 runs unprotected — upgrade every process that reads this file first\n"
                    .to_owned(),
                format!(
                    "{{ field = \"{}\", terminal = {}, absent = \"{}\" }}",
                    p.field, p.terminal, absent
                ),
            )
        },
    );
    format!(
        "[target.\"{}\"]   # keel doctor: {} — {}\n\
         {caveat}\
         timeout = \"30s\"\n\
         poll    = {{ interval = \"{}\", deadline = \"{}\", until = {} }}\n",
        p.key, p.note, provenance, interval, deadline, until
    )
}

/// What doctor does about ONE proposal, given what the project's `keel.toml`
/// already says about that route key (#139, spec §4.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RouteKeyPlan {
    /// No `[target."<key>"]` section at all: append the whole rendered block.
    Append,
    /// The section exists and carries a `poll` whose `until.absent` is missing
    /// — the inert block #139 is about. Set that one key to the carried value;
    /// appending would duplicate the section, and staying silent leaves a poll
    /// that never polls. The value rides along so the op cannot be built for a
    /// proposal that has none.
    AmendAbsent(&'static str),
}

/// The three-way verdict for one sighting's route keys (#139): what to act on,
/// and the keys doctor deliberately declined to touch.
struct RouteKeyPlans {
    /// Each proposal doctor will act on, paired with how.
    acting: Vec<(RouteKeyProposal, RouteKeyPlan)>,
    /// Keys this project already declares as a `[target."…"]` section carrying
    /// no usable `poll` table. Doctor will not duplicate the section, and
    /// writing a poll policy into it is a separate decision — so the finding
    /// names them rather than going quiet.
    unpolled: Vec<&'static str>,
}

impl RouteKeyPlans {
    /// The sorted key set the acted-on proposals form — the dedupe identity
    /// and the [`RouteCadence`] fold key (see [`simplification_findings`]).
    fn keys(&self) -> Vec<&'static str> {
        let mut keys: Vec<&'static str> = self.acting.iter().map(|(p, _)| p.key).collect();
        keys.sort_unstable();
        keys
    }

    fn amending(&self) -> bool {
        self.acting
            .iter()
            .any(|(_, plan)| matches!(plan, RouteKeyPlan::AmendAbsent(_)))
    }

    fn appending(&self) -> bool {
        self.acting
            .iter()
            .any(|(_, plan)| matches!(plan, RouteKeyPlan::Append))
    }
}

/// Classify one proposal against the project's existing document. `None` means
/// "leave this key alone"; [`section_lacks_poll`] then says WHICH kind of
/// leaving-alone it was, since only one of the two is worth reporting.
fn route_key_plan(existing: &toml_edit::DocumentMut, p: &RouteKeyProposal) -> Option<RouteKeyPlan> {
    let Some(section) = existing
        .get("target")
        .and_then(toml_edit::Item::as_table_like)
        .and_then(|t| t.get(p.key))
    else {
        return Some(RouteKeyPlan::Append);
    };
    let until = section
        .as_table_like()
        .and_then(|t| t.get("poll"))
        .and_then(toml_edit::Item::as_table_like)
        .and_then(|poll| poll.get("until"))
        .and_then(toml_edit::Item::as_table_like);
    let Some(until) = until else {
        // No `poll`, or a `poll` with no `until` (already schema-invalid; the
        // policy finding owns that file). Either way there is no inert poll
        // here to repair — see `RouteKeyPlans::unpolled`.
        return None;
    };
    // `until.field` is required by the schema; amending a document that lacks
    // it would write `absent` onto an `until` that still does not validate.
    if until.get("absent").is_some() || until.get("field").is_none() {
        return None;
    }
    // A route whose absence is not known to mean pending has nothing to amend.
    Some(RouteKeyPlan::AmendAbsent(p.absent?))
}

/// Whether the existing `[target."<key>"]` section carries no usable `poll`
/// table — the "different conversation" arm of the §4.2 table.
fn section_lacks_poll(existing: &toml_edit::DocumentMut, key: &str) -> bool {
    existing
        .get("target")
        .and_then(toml_edit::Item::as_table_like)
        .and_then(|t| t.get(key))
        .and_then(toml_edit::Item::as_table_like)
        .is_some_and(|section| {
            section
                .get("poll")
                .and_then(toml_edit::Item::as_table_like)
                .is_none()
        })
}

/// The route keys one `hand-rolled-poll` sighting proposes against
/// `policy_text`, each classified against what that document ACTUALLY
/// configures for it — not merely whether the section exists (#139): absent ⇒
/// append the block, present-but-inert (`poll` without `until.absent`) ⇒ amend
/// that one key, present and already carrying `absent` ⇒ leave alone,
/// present with no `poll` ⇒ leave alone and name it.
///
/// `None` when there is no base document to edit (absent or invalid
/// `keel.toml`: propose nothing rather than a patch that would create/clobber
/// the file), or when the sighting names no proposable route at all.
fn route_key_candidates(
    s: &SimplificationSighting,
    policy_text: Option<&str>,
    detected: &[crate::surface::Surface],
) -> Option<RouteKeyPlans> {
    let existing: toml_edit::DocumentMut = policy_text?.parse().ok()?;
    let proposals: Vec<RouteKeyProposal> = s
        .targets
        .iter()
        .flat_map(|t| route_key_proposals_for(t, &s.sdk_polls, detected))
        .collect();
    if proposals.is_empty() {
        return None;
    }
    let mut plans = RouteKeyPlans {
        acting: Vec::new(),
        unpolled: Vec::new(),
    };
    for p in proposals {
        match route_key_plan(&existing, &p) {
            Some(plan) => plans.acting.push((p, plan)),
            None => {
                if section_lacks_poll(&existing, p.key) {
                    plans.unpolled.push(p.key);
                }
            }
        }
    }
    Some(plans)
}

/// The applyable route-key proposal for one `hand-rolled-poll` sighting plus
/// the key set it covers. `None` when no shape is known, every proposed key is
/// already correctly configured, or the resulting document would not parse.
/// The cadence is looked up by key set, NOT read off `s` — see
/// [`RouteCadence`]. `detected` is the Google surface verdict the proposals are
/// narrowed to (see [`route_key_proposals_for`]).
fn route_key_fix(
    s: &SimplificationSighting,
    policy_text: Option<&str>,
    cadences: &BTreeMap<Vec<&'static str>, RouteCadence>,
    detected: &[crate::surface::Surface],
) -> Option<(Vec<&'static str>, Proposal)> {
    let plans = route_key_candidates(s, policy_text, detected)?;
    if plans.acting.is_empty() {
        return None;
    }
    let keys = plans.keys();
    let cadence = cadences.get(&keys).copied().unwrap_or_default();
    let ops: Vec<PolicyOp> = plans
        .acting
        .iter()
        .map(|(p, plan)| match plan {
            RouteKeyPlan::Append => PolicyOp::AppendBlock {
                text: render_route_block(p, s, cadence),
            },
            RouteKeyPlan::AmendAbsent(absent) => PolicyOp::Set {
                path: PolicyPath::new(["target", p.key, "poll", "until", "absent"]),
                value: (*absent).into(),
            },
        })
        .collect();
    let proposal = propose(policy_text, &ops).ok()?;
    (!proposal.patch.is_empty()).then_some((keys, proposal))
}

/// Fold every `hand-rolled-poll` sighting into the cadence of the route-key
/// set it would propose, BEFORE any finding is rendered — the patch the first
/// sighting carries has to speak for all of them. `detected` is passed through
/// to [`route_key_candidates`] so the fold keys match the key sets the emitted
/// patches actually carry.
fn route_cadences(
    scan: &ScanResult,
    policy_text: Option<&str>,
    detected: &[crate::surface::Surface],
) -> BTreeMap<Vec<&'static str>, RouteCadence> {
    let mut out: BTreeMap<Vec<&'static str>, RouteCadence> = BTreeMap::new();
    for s in &scan.simplifications {
        if s.kind != "hand-rolled-poll" {
            continue;
        }
        let Some(plans) = route_key_candidates(s, policy_text, detected) else {
            continue;
        };
        if plans.acting.is_empty() {
            continue;
        }
        out.entry(plans.keys())
            .and_modify(|c| c.fold(s))
            .or_insert_with(|| RouteCadence::of(s));
    }
    out
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
///
/// Poll v2: a `hand-rolled-poll` may also carry an applyable route-key `poll`
/// proposal. Two sightings of the same provider shape would propose the SAME
/// blocks against the same base file, and only one such patch can apply — so
/// the proposal is attached to the FIRST sighting (in `scan.simplifications`
/// order: file, line, kind) for a given key set, and later sightings with that
/// same key set point at it through `fix_ref` — the holder's `file:line`, a
/// structured reference rather than prose about report order (#107). Dedupe is
/// by key SET, not by target, so a google-genai loop and an openai loop each
/// still get their own patch.
///
/// `policy_text` is the base document a proposal edits; the caller passes
/// `None` for an invalid `keel.toml` — the removal fix on the policy finding
/// owns that file until it parses.
///
/// Google's two generative-AI surfaces share one target name, so the Google
/// route-key proposals are filtered to the surface this project actually uses
/// (see [`route_key_proposals_for`]). That detection is made HERE, once, and
/// returned alongside the findings: `keel doctor --json` reports the same
/// verdict, and a second detection call site would be a second source of
/// truth that could go stale against this one.
fn simplification_findings(
    scan: &ScanResult,
    topology: &Topology,
    policy_text: Option<&str>,
) -> (Vec<Finding>, crate::surface::SurfaceEvidence) {
    let surfaces = crate::surface::detect_surfaces(
        &crate::surface::policy_hosts(policy_text),
        &crate::surface::scan_hosts(scan),
    );
    let detected = surfaces.detected.clone();
    let wrappable: BTreeSet<&str> = topology.wrappable.iter().map(String::as_str).collect();
    let mut findings = Vec::new();
    // Key sets already proposed in this report -> the `file:line` of the
    // sighting whose finding carries that patch (the `fix_ref` a later
    // duplicate points at, so the pointer survives reordering/filtering).
    let mut proposed: BTreeMap<Vec<&'static str>, String> = BTreeMap::new();
    let cadences = route_cadences(scan, policy_text, &detected);
    for s in &scan.simplifications {
        let mut fix_ref = None;
        let targets = s.targets.join(", ");
        let actionable_now = s.targets.iter().any(|t| wrappable.contains(t.as_str()));
        let (level, when) = if actionable_now {
            ("warn", "Keel can wrap this target now")
        } else {
            ("info", "once Keel can wrap this target")
        };
        let mut fix = None;
        let (topic, what, action): (&'static str, String, String) = match s.kind.as_str() {
            "hand-rolled-poll" => {
                let plans = route_key_candidates(s, policy_text, &detected);
                // The first sighting for a key set carries the patch; later
                // ones name it, since only one of two identical patches can
                // apply against the same base file.
                if let Some((keys, proposal)) = route_key_fix(s, policy_text, &cadences, &detected)
                {
                    if let Some(holder) = proposed.get(&keys) {
                        fix_ref = Some(holder.clone());
                    } else {
                        proposed.insert(keys, format!("{}:{}", s.file, s.line));
                        fix = Some(proposal);
                    }
                }
                let mut action = "Wrap the target, then replace the loop with a `poll` policy — \
                     `poll.deadline` bounds the whole loop, `timeout` bounds one attempt. A \
                     POST-shaped operation read (Vertex `:fetch*Operation`) polls too: put \
                     `poll` on a route key (`[target.\"POST \
                     *-aiplatform.googleapis.com/*:fetchPredictOperation\"]`), which beats the \
                     LLM host map for that route."
                    .to_owned();
                if fix.is_some() {
                    // One patch can carry both an appended block and an amend
                    // to a block the project already has, so each clause is
                    // emitted only when it is true of THIS patch.
                    let appending = plans.as_ref().is_some_and(RouteKeyPlans::appending);
                    action.push_str(" Or apply the attached patch (`git apply`):");
                    if appending {
                        action.push_str(
                            " it adds the route-key `poll` block for this provider — tune \
                             `interval`/`deadline` to the job.",
                        );
                    }
                    if plans.as_ref().is_some_and(RouteKeyPlans::amending) {
                        // #139: the operator already adopted the route key, so
                        // `keel status` attributes the calls and the block
                        // validates — and it still never polls. Say that here,
                        // not in a footnote.
                        action.push_str(if appending {
                            " It ALSO sets"
                        } else {
                            " it sets"
                        });
                        action.push_str(
                            " `until.absent = \"pending\"` on the route-key `poll` block this \
                             project already declares — as written, that block returns on the \
                             FIRST response and does not poll at all, because a running \
                             `google.longrunning.Operation` omits `done` entirely (proto3 JSON \
                             drops a false bool).",
                        );
                    }
                } else if let Some(holder) = &fix_ref {
                    let _ = write!(
                        action,
                        " The route-key patch for this provider is attached to the \
                         `hand-rolled-poll` finding for {holder} (`fix_ref`)."
                    );
                }
                // A route key the project declares with no `poll` table: doctor
                // will not duplicate the section, and it will not guess a poll
                // policy into someone else's block either. Name it instead of
                // going quiet — silence here is exactly the #139 failure.
                if fix_ref.is_none()
                    && let Some(unpolled) = plans.as_ref().map(|p| p.unpolled.as_slice())
                    && !unpolled.is_empty()
                {
                    let names = unpolled
                        .iter()
                        .map(|k| format!("`[target.\"{k}\"]`"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let _ = write!(
                        action,
                        " This project already declares {names} with no `poll` table — doctor \
                         leaves that section alone; adding poll-until-terminal to it is a \
                         separate decision."
                    );
                }
                (
                    "hand-rolled-poll",
                    format!(
                        "`{}` ({}:{}) hand-rolls poll-until-terminal against `{}` — {}, a `poll` \
                         policy (interval / deadline / until) replaces the whole loop",
                        s.function, s.file, s.line, targets, when
                    ),
                    action,
                )
            }
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
            fix,
            fix_ref,
            level,
            topic,
        });
    }
    (findings, surfaces)
}

/// The rank table: ascending Keel-confidence. Rank 1 (url-no-transport) is
/// the claim Keel knows least about — it saw a URL but cannot even name the
/// dispatch path — so it is investigated first. Rank 2
/// (orchestration-blind-spot) is a coarse substring match on a file Keel
/// cannot parse at all — strictly less verifiable than rank 3
/// (subprocess-blind-spot), which comes from an AST sighting of a real call
/// — so it sorts above it. Rank 4 covers EVERY `topology.excluded` kind
/// (dependency-averse-excluded; since #64, local-host-excluded; since WS5,
/// reserved-name-excluded and test-only-excluded) — same confidence tier,
/// "Keel saw why this was excluded and just wants it confirmed", ties broken
/// by `code` then `subject`. Rank 5 also covers
/// `sdk-client-timeout` (since #80): a mechanically-verified fact from the
/// declared policy itself (Keel is fully confident an LRO-sized timeout is
/// set), same tier as `preexisting-resilience`'s "Keel is confident about
/// what it saw, a human still has to decide", ties again broken by `code`
/// then `subject`. Rank 6 (code-hash-stale, reserved for the WS6 emitter) is
/// a mechanical, fully-verified fact that merely awaits a human decision.
fn follow_up_rank(code: &str) -> u32 {
    match code {
        "url-no-transport" => 1,
        "orchestration-blind-spot" => 2,
        "subprocess-blind-spot" => 3,
        "dependency-averse-excluded"
        | "local-host-excluded"
        | "reserved-name-excluded"
        | "test-only-excluded" => 4,
        "preexisting-resilience" | "sdk-client-timeout" => 5,
        _ => 6, // code-hash-stale (WS6)
    }
}

/// The ranked follow-up list (WS2): computed from the same structured
/// evidence as the findings — never by parsing finding text — then sorted by
/// the stable key (rank, code, subject).
#[allow(clippy::too_many_lines)] // one section per follow-up code, straight-line;
// issue #80 added the `sdk-client-timeout` section, not new complexity.
fn build_follow_ups(
    topology: &Topology,
    resilience: Option<&Finding>,
    scan: &ScanResult,
    stale_flows: &[crate::flows::StaleFlow],
    lro_timeouts: &[(String, u64)],
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
    // WS5: a launch seen only in test files is not a production blind spot
    // either — it stays out of the detail list and is surfaced only as a count
    // on the subject, so the reader knows the evidence was seen, not dropped.
    let (in_tests, unmatched): (Vec<&ExternalProcess>, Vec<&ExternalProcess>) = topology
        .external_processes
        .iter()
        .filter(|p| p.covered_by.is_none())
        .partition(|p| p.in_tests);
    // Issue #91: a Python child that inherits our environment self-activates
    // via the keelrun `.pth` when active — not a blind spot to chase, so it
    // is counted in the subject (evidence is not dropped) but never listed
    // in the detail.
    let (inheriting, uncovered): (Vec<&ExternalProcess>, Vec<&ExternalProcess>) = unmatched
        .into_iter()
        .partition(|p| p.inherits_activation == Some("python-pth"));
    // Issue #102: even when every sighting self-activates (no uncovered blind
    // spot at all), the inheriting count must still be reachable from
    // `follow_ups`, not only from the lower-priority `info` finding — a
    // project whose children ALL self-activate previously never saw this
    // follow-up. The detail branches on whether there is anything left to
    // chase; the subject/suffix formatting for the already-working uncovered
    // case is unchanged.
    if !uncovered.is_empty() || !inheriting.is_empty() {
        let mut extra = Vec::new();
        if !in_tests.is_empty() {
            extra.push(format!("+{} in test files", in_tests.len()));
        }
        if !inheriting.is_empty() {
            let (noun, verb) = if inheriting.len() == 1 {
                ("child", "self-activates")
            } else {
                ("children", "self-activate")
            };
            extra.push(format!(
                "+{} Python {noun} that {verb} when it inherits KEEL_ENABLE",
                inheriting.len()
            ));
        }
        let suffix = if extra.is_empty() {
            String::new()
        } else {
            format!(" ({})", extra.join(", "))
        };
        let detail = if uncovered.is_empty() {
            let (noun, verb) = if inheriting.len() == 1 {
                ("child", "self-activates")
            } else {
                ("children", "self-activate")
            };
            format!(
                "No externally-launched process here is a blind spot — the {} Python {noun} \
                 that {verb} when it inherits KEEL_ENABLE. There is no blind spot to chase, \
                 only to confirm the self-activation covers what you expect.",
                inheriting.len()
            )
        } else {
            let cmds: Vec<String> = uncovered
                .iter()
                .map(|p| format!("`{}` ({}:{})", p.command, p.file, p.line))
                .collect();
            format!(
                "Keel cannot see traffic inside externally-launched processes; confirm none of \
                 these carry traffic you care about: {}.",
                cmds.join(", ")
            )
        };
        ups.push(FollowUp {
            code: "subprocess-blind-spot",
            detail,
            rank: follow_up_rank("subprocess-blind-spot"),
            subject: format!(
                "{} externally-launched process(es){suffix}",
                uncovered.len()
            ),
        });
    }
    for entry in &topology.excluded {
        let code = excluded_kind_topic(entry.kind);
        ups.push(FollowUp {
            code,
            detail: format!(
                "`{}` was excluded from proposed policy — {}. Confirm the exclusion is intended.",
                entry.host, entry.reason
            ),
            rank: follow_up_rank(code),
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
    for (subject, ms) in lro_timeouts {
        ups.push(FollowUp {
            code: "sdk-client-timeout",
            rank: follow_up_rank("sdk-client-timeout"),
            subject: subject.clone(),
            detail: format!(
                "timeout = {}s bounds ONE attempt of ONE call, and is beyond the client-default \
                 deadline most SDKs enforce (often ~600s). Keel wraps the transport; it does not \
                 raise the SDK's own deadline — the call site must also pass a timeout >= the \
                 Keel value, or the SDK gives up first and Keel just sees a retryable timeout. \
                 For a submit-then-poll API a `poll` policy (whose `deadline` bounds the WHOLE \
                 loop) replaces the loop; a POST-shaped operation read (Vertex \
                 `:fetch*Operation`) takes it on a route key — see `hand-rolled-poll` findings \
                 for an applyable patch.",
                ms / 1000
            ),
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

/// A `keel.toml` outside the agent directory of a Google `agents-cli` project
/// (an `agents-cli-manifest.yaml` naming an `agent_directory`) never reaches
/// the container: the generated Dockerfile only `COPY`s `pyproject.toml`,
/// `README.md`, `uv.lock*`, and the agent directory itself. Emitted only when
/// a manifest is found, `<project>/keel.toml` actually exists, and that file is
/// NOT inside the agent directory — a policy already under `agent_dir` ships
/// fine, which covers both the `agent_directory`-names-the-project-root case
/// and (since issue #87 made the upward walk actually work) a `keel.toml` in a
/// subdirectory of the agent directory.
///
/// Containment is decided on CANONICALIZED paths, the house pattern from
/// `init::agents_cli_toml_path`: `project` is the relative `"."` `main.rs`
/// passes, while a manifest found above it yields an absolute `agent_dir`, so
/// a syntactic comparison of the two would be meaningless. Fails open — if
/// either side cannot be canonicalized (a TOCTOU removal), say nothing rather
/// than guess.
fn agents_cli_placement_finding(project: &Path) -> Option<Finding> {
    let layout = agents_cli::find_agents_cli_layout(project)?;
    let keel_toml = evidence::keel_toml(project);
    if !keel_toml.exists() {
        return None;
    }
    let canonical_toml = std::fs::canonicalize(&keel_toml).ok()?;
    let canonical_agent_dir = std::fs::canonicalize(&layout.agent_dir).ok()?;
    if canonical_toml.starts_with(&canonical_agent_dir) {
        return None;
    }
    // Display paths relative to the MANIFEST directory — the agents-cli
    // project root, which is the anchor `agent_directory` is itself declared
    // against and the only one that is stable at every walk level. Relative to
    // `project` would be identical at level 0 (manifest_dir IS project there)
    // but degrade to an absolute machine path once the walk climbs, since
    // `agent_dir` is then never under `project` — and the whole point of this
    // is to keep the finding's text, and therefore `--json`, reproducible
    // across checkouts instead of embedding wherever this particular clone
    // happens to sit on disk.
    let agent_dir = relative_display(&layout.manifest_dir, &layout.agent_dir);
    let moved_to = relative_display(&layout.manifest_dir, &layout.agent_dir.join("keel.toml"));
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
        fix_ref: None,
        level: "warn",
        topic: "agents-cli-config-placement",
    })
}

/// Bound on how many parents [`config_above_cwd_finding`] walks above
/// `project` — mirrors `agents_cli::find_agents_cli_layout`'s bounded-walk
/// shape (same bound, 8), so neither walk can loop forever on a pathological
/// filesystem.
const CONFIG_ABOVE_CWD_MAX_WALK_LEVELS: usize = 8;

/// Issue #85: a `keel.toml` sitting in a directory *above* `project` is
/// invisible to this report and to runtime activation — both resolve config
/// from their own working directory only, never by searching upward. Emitted
/// only when `project` itself has no `keel.toml` (checked here directly, not
/// just by the caller's `!policy.present` gate, so this function is correct
/// standalone too — a parent's `keel.toml` is irrelevant once the project has
/// its own). Fails open: any filesystem error (an unreadable/unwalkable
/// ancestor, a `project` with no parent) yields `None` rather than a wrong
/// finding.
///
/// Canonicalize BEFORE walking, not after: `main.rs` hands every subcommand
/// `project = Path::new(".")`, and `Path::new(".").parent()` is `Some("")`
/// while `Path::new("").parent()` is `None` — so a relative-path walk dies one
/// step in, without ever leaving the project directory, and this finding could
/// never fire in production. Resolving to an absolute path first makes the walk
/// real, and hands the message the absolute paths it wants for free.
fn config_above_cwd_finding(project: &Path) -> Option<Finding> {
    if project.join("keel.toml").is_file() {
        return None;
    }
    let here = std::fs::canonicalize(project).ok()?;
    let mut dir = here.parent()?;
    for _ in 0..CONFIG_ABOVE_CWD_MAX_WALK_LEVELS {
        if dir.join("keel.toml").is_file() {
            let parent = dir;
            return Some(Finding {
                action: format!(
                    "Run keel doctor from {} to report against that policy; for runtime \
                     activation, set KEEL_CWD={}. keel doctor reads its own working directory \
                     only.",
                    parent.display(),
                    parent.display()
                ),
                detail: format!(
                    "keel.toml found at {} but this report ran from {} — the policy file is \
                     NOT loaded from there.",
                    parent.display(),
                    here.display()
                ),
                fix: None,
                fix_ref: None,
                level: "warn",
                topic: "config-above-cwd",
            });
        }
        dir = dir.parent()?;
    }
    None
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
/// class. The `protocol` line enumerates the skill's six phases verbatim — if
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
        parsed_files: &["dockerfile (COPY/ADD directives only)"],
        parsed_languages: &["python", "js-ts"],
        protocol: "Static + adapter-interception evidence, not a verdict. Drive an \
                   evaluate/adopt/review task through the keel skill's six phases: Scope every \
                   I/O process (including shell/CI launchers) -> Explore how each call is \
                   dispatched -> Collect this report -> Baseline real failure classes in observe \
                   mode (`keel record run`) -> Analyze & propose -> Ship (policy in the artifact, \
                   activation reaching the I/O process, durable evidence). Retry only helps \
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
        fix_ref: None,
        level: "error",
        topic: "journal",
    })
}

/// Deployment artifacts at the project root that mean "this runs in a
/// container or on a serverless platform": the parsed build files plus the
/// common platform manifests. Root only, like the Dockerfile scan (WS4).
fn deploy_artifacts(project: &Path, build_files: &[crate::dockerfile::BuildFile]) -> Vec<String> {
    let mut out: Vec<String> = build_files.iter().map(|b| b.file.clone()).collect();
    for name in ["app.yaml", "fly.toml", "serverless.yaml", "serverless.yml"] {
        if project.join(name).is_file() {
            out.push(name.to_owned());
        }
    }
    out.sort();
    out.dedup();
    out
}

/// A durable-flow journal on SQLite at a deployment artifact's project root
/// (issue #90): the redeploy/scale-to-zero pattern that discards it is common
/// enough on container/serverless platforms that this is worth a warning
/// without any runtime evidence — doctor can see the artifact in the repo,
/// the runtime half of this check (below) sees the environment instead.
fn journal_ephemeral_finding(
    journal: &JournalReport,
    flows_configured: bool,
    artifacts: &[String],
) -> Option<Finding> {
    if journal.backend != "sqlite" || !flows_configured || artifacts.is_empty() {
        return None;
    }
    Some(Finding {
        action:
            "Mount a persistent volume at `.keel/` (or point `journal` at a `file:` path on one), \
                 or use a Postgres journal. Until then a redeploy or scale-to-zero discards every \
                 resumable flow."
                .to_owned(),
        detail: format!(
            "`[flows]` is configured and the journal is SQLite at `{}` — this project ships as a \
             container/serverless artifact ({}), whose filesystem does not survive an instance \
             replacement.",
            journal.location,
            artifacts.join(", ")
        ),
        fix: None,
        fix_ref: None,
        level: "warn",
        topic: "journal-ephemeral-storage",
    })
}

/// Canonicalize both sides before comparing, falling back to the raw
/// (non-canonicalized) form when a path does not exist on disk — a recorded
/// activation may point at a path that no longer exists, and a project under
/// test may not exist either; string equality is still a meaningful check in
/// that case.
fn same_path(a: &str, b: &Path) -> bool {
    let ca = std::fs::canonicalize(a).unwrap_or_else(|_| PathBuf::from(a));
    let cb = std::fs::canonicalize(b).unwrap_or_else(|_| b.to_path_buf());
    ca == cb
}

/// Whether any recorded activation row matches this project's resolved
/// policy identity (#92) — see the interface doc comment in the task brief
/// for the exact predicate. `"verified"` requires a row whose policy
/// identity (path when a `keel.toml` is present, else defaults-rooted-here)
/// matches; otherwise `"unverified"`, including when there are no rows at
/// all. Alongside the verdict, returns that row's `backend` (#129) — `None`
/// when unverified, or when the matching row predates the `backend` column.
fn runtime_activation(
    project: &Path,
    policy_present: bool,
    rows: &[Activation],
) -> (&'static str, Option<String>) {
    let matches = |r: &&Activation| {
        if policy_present {
            r.policy_path
                .as_deref()
                .is_some_and(|p| same_path(p, &project.join("keel.toml")))
        } else {
            r.policy_source == "defaults" && same_path(&r.cwd, project)
        }
    };
    match rows.iter().find(matches) {
        Some(r) => ("verified", r.backend.clone()),
        None => ("unverified", None),
    }
}

/// Assemble the report from the ten evidence inputs. Pure, so the golden test
/// pins it without a filesystem or `python3` — the filesystem-dependent
/// inputs (`agents_cli_finding`, since it needs to walk for a manifest and
/// check for a root `keel.toml`; `config_above_cwd_finding`, since it walks
/// parent directories for a `keel.toml` — issue #85; `boundaries`, since it
/// stats the project root for governance files; `build_files`, since it reads
/// the root `Dockerfile`s and `.dockerignore`; `stale_flows`, since it
/// needs to read `.keel/journal.db` and stat scripts on disk; `runtime_activation`,
/// since it is computed from a read of `.keel/discovery.db`'s activations
/// table — #92) are computed by the caller and passed in already resolved,
/// the same pattern `policy`/`journal` already use.
#[allow(clippy::too_many_lines)]
// straight-line report assembly, one section per
// DoctorReport field; issue #41 added the cmd_match plumbing, not new complexity.
#[allow(clippy::too_many_arguments)] // twelve already-resolved evidence inputs (see doc
// comment above); issue #85 added config_above_cwd_finding, WS3 added build_files,
// #92 added runtime_activation, issue #90 added artifacts, #129 added activation_backend.
fn build_report(
    scan: &ScanResult,
    wrapped_targets: &BTreeSet<String>,
    policy: PolicyValidation,
    journal: JournalReport,
    agents_cli_finding: Option<Finding>,
    config_above_cwd_finding: Option<Finding>,
    boundaries: Boundaries,
    build_files: &[crate::dockerfile::BuildFile],
    artifacts: &[String],
    stale_flows: &[crate::flows::StaleFlow],
    runtime_activation: &'static str,
    activation_backend: Option<String>,
) -> DoctorReport {
    let PolicyValidation {
        check: policy,
        cmd_match,
        lro_timeouts,
        fix,
        text: policy_text,
        flows_configured,
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
    // WS5: a host the topology already excluded (loopback, a reserved name, a
    // test-only sighting, a dependency-averse file) must not ALSO raise a
    // `visible-unwrapped` warn — that was the same host counted twice, once as
    // a warn and once as the info explaining why it is not a dependency. The
    // raw `coverage.visible_unwrapped` list is deliberately unchanged: it is
    // the unfiltered set-difference fact, not a finding.
    let excluded_hosts: BTreeSet<&str> =
        topology.excluded.iter().map(|e| e.host.as_str()).collect();
    for target in visible_unwrapped
        .iter()
        .filter(|t| !excluded_hosts.contains(t.as_str()))
    {
        findings.push(Finding {
            action:
                "Run `keel run <script>` so Keel can confirm this target is wrapped at runtime."
                    .to_owned(),
            detail: format!(
                "`{target}` is visible in your code but has no observed runtime evidence."
            ),
            fix: None,
            fix_ref: None,
            level: "warn",
            topic: "visible-unwrapped",
        });
    }
    for lib in &invisible {
        findings.push(Finding {
            action: format!("No adapter for `{lib}` yet — its calls are invisible to Keel. Track adapter support or wrap manually."),
            detail: format!("`{lib}` is imported but has no adapter in the registry."),
            fix: None,
            fix_ref: None,
            level: "warn",
            topic: "invisible",
        });
    }
    // Always: the honest advisory about what static + adapter interception can't see.
    findings.push(Finding {
        action: "If a dependency makes calls Keel never reports, file an adapter request.".to_owned(),
        detail: "Raw sockets and unknown native libraries are invisible to static and adapter-based interception.".to_owned(),
        fix: None,
        fix_ref: None,
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
            fix_ref: None,
            level: "warn",
            topic: "orchestration-blind-spot",
        });
    }
    findings.extend(topology_findings(&topology));
    // An invalid keel.toml already carries the removal fix on the policy
    // finding; a second patch against the same base file could not apply too.
    // The surface verdict is computed here and dropped for now; Task 6 reports
    // it in `keel doctor --json`.
    let (simplifications, _surfaces) = simplification_findings(
        scan,
        &topology,
        policy.valid.then_some(policy_text.as_deref()).flatten(),
    );
    findings.extend(simplifications);
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
            fix_ref: None,
            level: "error",
            topic: "policy",
        });
    }
    let resilience = resilience_finding(scan, &registry_libs);
    let follow_ups = build_follow_ups(
        &topology,
        resilience.as_ref(),
        scan,
        stale_flows,
        &lro_timeouts,
    );
    findings.extend(resilience);
    findings.extend(journal_finding(&journal));
    findings.extend(journal_ephemeral_finding(
        &journal,
        flows_configured,
        artifacts,
    ));
    findings.extend(agents_cli_finding);
    // WS3: only meaningful when there IS a policy file to ship — with no
    // keel.toml in the checkout there is nothing for the image to be missing.
    if policy.present {
        findings.extend(packaging_findings(build_files));
    }
    // Issue #85: only meaningful when this project has no keel.toml of its
    // own — `config_above_cwd_finding` already checks this independently,
    // but gating here too keeps the rule visible at the one call site that
    // decides what goes into the report.
    if !policy.present {
        findings.extend(config_above_cwd_finding);
    }

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
        runtime_activation,
        activation_backend,
        topology,
    }
}

/// WS3: the policy file exists in the checkout — does the image get it?
fn packaging_findings(build_files: &[crate::dockerfile::BuildFile]) -> Vec<Finding> {
    use crate::dockerfile::Reach;
    let mut out = Vec::new();
    for bf in build_files {
        match bf.reach {
            Reach::NotReached => out.push(Finding {
                action:
                    "Add `COPY keel.toml ./` (before the layer that runs the app). At runtime the \
                         policy is read from KEEL_CWD or the working directory; without the file \
                         Keel refuses to activate under KEEL_CWD and runs production defaults \
                         otherwise."
                        .to_owned(),
                detail: format!(
                    "`{}` has no COPY/ADD directive that reaches keel.toml — the policy in this \
                     checkout will not be in the image.",
                    bf.file
                ),
                fix: None,
                fix_ref: None,
                level: "warn",
                topic: "keel-toml-not-in-image",
            }),
            Reach::Reached if !bf.reached_in_final_stage => out.push(Finding {
                action:
                    "Copy keel.toml into the final stage too (`COPY keel.toml ./` after the last \
                         FROM), or `COPY --from=<stage>` it across."
                        .to_owned(),
                detail: format!(
                    "`{}` copies keel.toml only in a non-final build stage — the runtime image \
                     will not have it.",
                    bf.file
                ),
                fix: None,
                fix_ref: None,
                level: "warn",
                topic: "keel-toml-not-in-image",
            }),
            Reach::Indeterminate => out.push(Finding {
                action: "Confirm the expanded source includes keel.toml, or add an explicit \
                         `COPY keel.toml ./`."
                    .to_owned(),
                detail: format!(
                    "`{}`: could not tell whether keel.toml reaches the image — `{}` depends on a \
                     build variable.",
                    bf.file,
                    bf.directive.as_deref().unwrap_or_default()
                ),
                fix: None,
                fix_ref: None,
                level: "info",
                topic: "keel-toml-image-indeterminate",
            }),
            Reach::Reached => {}
        }
    }
    out
}

/// RFC 2606 reserved names (`example.com/net/org`, `.example`, `.test`,
/// `.invalid`, `.localhost`) and RFC 5737 / RFC 3849 documentation address
/// ranges: fixtures by definition, never a dependency (WS5).
fn reserved_name(host: &str) -> bool {
    const HOSTS: &[&str] = &["example.com", "example.net", "example.org"];
    const TLDS: &[&str] = &["example", "test", "invalid", "localhost"];
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    if let Ok(ip) = h.parse::<std::net::IpAddr>() {
        return match ip {
            std::net::IpAddr::V4(v4) => {
                let o = v4.octets();
                matches!(
                    (o[0], o[1], o[2]),
                    (192, 0, 2) | (198, 51, 100) | (203, 0, 113)
                )
            }
            std::net::IpAddr::V6(v6) => {
                let s = v6.segments();
                s[0] == 0x2001 && s[1] == 0x0db8
            }
        };
    }
    let last_label = h.rsplit('.').next().unwrap_or("");
    HOSTS
        .iter()
        .any(|r| h == *r || h.ends_with(&format!(".{r}")))
        || TLDS.contains(&last_label)
}

/// Sort every host the static scan saw into exactly one of the three honesty
/// buckets (dx-spec §2 — "wrap it" / "can't reach it" / "shouldn't reach
/// it"), plus the host-independent external-process signal. Precedence: a
/// wrapped-at-runtime target or an `llm:*` target is wrappable by
/// construction regardless of transport class (runtime evidence, or the LLM
/// pack's own wrapping, beats static doubt); otherwise a statically-seen
/// `localhost`/loopback/unspecified target (#64 — e.g. a test server, not a
/// real dependency) is excluded ahead of any other check; otherwise an RFC
/// 2606/5737 reserved or documentation name, then a target seen ONLY inside
/// test files, are excluded (WS5 — fixtures, not dependencies); otherwise a
/// target seen ONLY inside a dependency-averse file is excluded (shouldn't
/// reach it) ahead of any transport check; otherwise the transport class decides
/// wrappable (tracked) vs. unreachable (untracked-known/unknown). `pub(crate)`:
/// `init.rs` reuses this directly for `keel init --diff` to skip proposing
/// policy for excluded hosts and print why (passing an empty `cmd_match` —
/// `--diff` never touches `external_processes`, so cross-referencing it
/// there would be dead work).
#[allow(clippy::too_many_lines)] // one straight-line exclusion check per bucket,
// in documented precedence order; WS5 added two more, not new branching depth.
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
        let local_only = target == "localhost"
            || target
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback() || ip.is_unspecified());
        if local_only {
            excluded.push(TopologyEntry {
                host: target.clone(),
                kind: "local/loopback",
                reason: "local/loopback host — run under keel to gather runtime evidence, or \
                         add it to keel.toml explicitly"
                    .to_owned(),
            });
            continue;
        }
        if reserved_name(target) {
            excluded.push(TopologyEntry {
                host: target.clone(),
                kind: "reserved-name",
                reason: "RFC 2606/5737 reserved or documentation name — a fixture, not a \
                         dependency; add it to keel.toml explicitly if it is real"
                    .to_owned(),
            });
            continue;
        }
        let test_only =
            !ev.sightings.is_empty() && ev.sightings.iter().all(|s| scan::is_test_path(&s.file));
        if test_only {
            let files: BTreeSet<&str> = ev.sightings.iter().map(|s| s.file.as_str()).collect();
            excluded.push(TopologyEntry {
                host: target.clone(),
                kind: "test-only",
                reason: format!(
                    "seen only in test file(s) {} — run under keel to gather runtime evidence, \
                     or add it to keel.toml explicitly",
                    files.into_iter().collect::<Vec<_>>().join(", ")
                ),
            });
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
                kind: "dependency-averse",
                reason: format!(
                    "seen only in dependency-averse file(s) {} — add `# keel: include` to \
                     override",
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
                kind: "untracked-transport",
                reason: "reached via a stdlib transport Keel does not adapt (http.client, or \
                         urllib without urllib.request; Python's urllib.request itself is \
                         adapted)"
                    .to_owned(),
            }),
            TransportClass::Unknown => unreachable.push(TopologyEntry {
                host: target.clone(),
                kind: "unknown-transport",
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
            in_tests: scan::is_test_path(&s.file),
            launcher: s.launcher.clone(),
            line: s.line,
            child_runtime: s.child_runtime.clone(),
            inherits_activation: inherits_activation(s),
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

/// LRO-sized: >10min. Most SDK client-default deadlines are <=600s, so a
/// declared `timeout` past this point is very likely to be beaten by the
/// SDK's own deadline before Keel's ever fires (issue #80).
const LRO_TIMEOUT_MS: u64 = 600_000;

/// Validate `keel.toml` against the typed [`Policy`] model, reporting the exact
/// field path on error (via `serde_path_to_error`) and, when a field is at
/// fault, attaching the applyable removal fix.
fn validate_policy(path: &Path) -> PolicyValidation {
    if !path.exists() {
        return PolicyValidation {
            check: PolicyCheck {
                field: None,
                message: None,
                path: None,
                present: false,
                valid: true,
            },
            cmd_match: BTreeMap::new(),
            lro_timeouts: Vec::new(),
            fix: None,
            text: None,
            flows_configured: false,
        };
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return invalid(None, "keel.toml exists but could not be read", None, None);
    };
    let toml_value: toml::Value = match text.parse() {
        Ok(v) => v,
        Err(e) => {
            return invalid(
                None,
                &format!("keel.toml is not valid TOML: {e}"),
                None,
                Some(text),
            );
        }
    };
    let json_value = match serde_json::to_value(&toml_value) {
        Ok(v) => v,
        Err(e) => {
            return invalid(
                None,
                &format!("keel.toml could not be normalized: {e}"),
                None,
                Some(text),
            );
        }
    };
    match serde_path_to_error::deserialize::<_, Policy>(&json_value) {
        Ok(policy) => {
            // Issue #80: collect every declared timeout past the LRO-sized
            // threshold before `policy` moves — `policy.target` is a
            // `BTreeMap`, so this iteration order is already deterministic.
            let mut lro_timeouts: Vec<(String, u64)> = Vec::new();
            if let Some(t) = policy.defaults.outbound.as_ref().and_then(|d| d.timeout)
                && t.0 > LRO_TIMEOUT_MS
            {
                lro_timeouts.push(("defaults.outbound".to_string(), t.0));
            }
            if let Some(t) = policy.defaults.llm.as_ref().and_then(|d| d.timeout)
                && t.0 > LRO_TIMEOUT_MS
            {
                lro_timeouts.push(("defaults.llm".to_string(), t.0));
            }
            for (name, tp) in &policy.target {
                if let Some(t) = tp.timeout
                    && t.0 > LRO_TIMEOUT_MS
                {
                    lro_timeouts.push((format!("target.\"{name}\""), t.0));
                }
            }
            // Issue #90: computed BEFORE `policy.flows` moves into `cmd_match`
            // below.
            let flows_configured = policy.flows.as_ref().is_some_and(|f| {
                !f.entrypoints.is_empty() || f.match_.as_ref().is_some_and(|m| !m.is_empty())
            });
            PolicyValidation {
                check: PolicyCheck {
                    field: None,
                    message: None,
                    path: Some("keel.toml".to_owned()),
                    present: true,
                    valid: true,
                },
                cmd_match: policy.flows.and_then(|f| f.match_).unwrap_or_default(),
                lro_timeouts,
                fix: None,
                text: Some(text),
                flows_configured,
            }
        }
        Err(e) => {
            let field = e.path().to_string();
            let fix = suggest_removal(&text, &field);
            invalid(Some(field), &e.inner().to_string(), fix, Some(text))
        }
    }
}

fn invalid(
    field: Option<String>,
    message: &str,
    fix: Option<Proposal>,
    text: Option<String>,
) -> PolicyValidation {
    PolicyValidation {
        check: PolicyCheck {
            field,
            message: Some(message.to_owned()),
            path: Some("keel.toml".to_owned()),
            present: true,
            valid: false,
        },
        cmd_match: BTreeMap::new(),
        lro_timeouts: Vec::new(),
        fix,
        text,
        flows_configured: false,
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
    let _ = writeln!(out, "  runtime activation: {}", r.runtime_activation);
    if let Some(b) = &r.activation_backend {
        let _ = writeln!(out, "  activation backend: {b}");
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

    #[test]
    fn journal_ephemeral_finding_needs_sqlite_flows_and_an_artifact() {
        let sqlite = default_journal();
        let bf = vec![crate::dockerfile::BuildFile {
            file: "Dockerfile".into(),
            reach: crate::dockerfile::Reach::Reached,
            directive: None,
            reached_in_final_stage: true,
        }];
        assert!(journal_ephemeral_finding(&sqlite, true, &["Dockerfile".to_owned()]).is_some());
        assert!(
            journal_ephemeral_finding(&sqlite, false, &["Dockerfile".to_owned()]).is_none(),
            "no flows → nothing durable to lose"
        );
        assert!(
            journal_ephemeral_finding(&sqlite, true, &[]).is_none(),
            "no artifact → not a deployment we can see"
        );
        let pg = JournalReport {
            backend: "postgres",
            location: "postgres://\u{2026}".into(),
            source: "keel.toml",
            supported: false,
        };
        assert!(journal_ephemeral_finding(&pg, true, &["Dockerfile".to_owned()]).is_none());
        let f = journal_ephemeral_finding(
            &sqlite,
            true,
            &["Dockerfile".to_owned(), "fly.toml".to_owned()],
        )
        .unwrap();
        assert_eq!((f.topic, f.level), ("journal-ephemeral-storage", "warn"));
        assert!(f.detail.contains("Dockerfile, fly.toml"), "{}", f.detail);
        let _ = bf;
    }

    #[test]
    fn deploy_artifacts_lists_root_files_only() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("fly.toml"), "").unwrap();
        std::fs::write(dir.path().join("serverless.yml"), "").unwrap();
        std::fs::create_dir(dir.path().join("deploy")).unwrap();
        std::fs::write(dir.path().join("deploy/app.yaml"), "").unwrap();
        let bf = crate::dockerfile::analyze(dir.path());
        assert_eq!(
            deploy_artifacts(dir.path(), &bf),
            vec!["fly.toml".to_owned(), "serverless.yml".to_owned()]
        );
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
    fn runtime_activation_is_verified_only_by_a_matching_row() {
        let dir = tempfile::TempDir::new().unwrap();
        let project = dir.path();
        std::fs::write(project.join("keel.toml"), "").unwrap();
        let row = |src: &str, path: Option<String>, cwd: String| Activation {
            ts_ms: 1,
            pid: 1,
            language: "python".into(),
            version: "x".into(),
            cwd,
            keel_cwd: None,
            policy_source: src.into(),
            policy_path: path,
            flows_configured: false,
            argv0: String::new(),
            backend: Some("native".into()),
        };
        let here = project.to_string_lossy().into_owned();
        let mine = row(
            "keel.toml",
            Some(project.join("keel.toml").to_string_lossy().into_owned()),
            here.clone(),
        );
        let elsewhere = row(
            "keel.toml",
            Some("/somewhere/else/keel.toml".into()),
            "/somewhere/else".into(),
        );
        assert_eq!(
            runtime_activation(project, true, std::slice::from_ref(&mine)),
            ("verified", Some("native".to_owned()))
        );
        assert_eq!(
            runtime_activation(project, true, std::slice::from_ref(&elsewhere)),
            ("unverified", None)
        );
        assert_eq!(runtime_activation(project, true, &[]), ("unverified", None));
        // A project with no keel.toml is verified by a defaults activation rooted here.
        std::fs::remove_file(project.join("keel.toml")).unwrap();
        assert_eq!(
            runtime_activation(project, false, &[row("defaults", None, here)]),
            ("verified", Some("native".to_owned()))
        );
        assert_eq!(
            runtime_activation(project, false, &[mine]),
            ("unverified", None)
        );
        // A verified row written before #129 has no backend column value.
        let no_backend = Activation {
            backend: None,
            ..row(
                "keel.toml",
                Some(project.join("keel.toml").to_string_lossy().into_owned()),
                project.to_string_lossy().into_owned(),
            )
        };
        std::fs::write(project.join("keel.toml"), "").unwrap();
        assert_eq!(
            runtime_activation(project, true, &[no_backend]),
            ("verified", None)
        );
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
                path: None,
                present: false,
                valid: true,
            },
            cmd_match: BTreeMap::new(),
            lro_timeouts: Vec::new(),
            fix: None,
            text: None,
            flows_configured: false,
        };
        let r = build_report(
            &scan,
            &wrapped,
            policy,
            default_journal(),
            None,
            None,
            empty_boundaries(),
            &[],
            &[],
            &[],
            "unverified",
            None,
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
    #[allow(clippy::too_many_lines)] // straight-line fixture setup, one bucket per
    // scan.targets insert; the extra build_report artifacts arg pushed this over 100.
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
            child_runtime: None,
            env_inheritance: "inherited".into(),
        });
        let r = build_report(
            &scan,
            &BTreeSet::new(),
            default_policy(),
            default_journal(),
            None,
            None,
            empty_boundaries(),
            &[],
            &[],
            &[],
            "unverified",
            None,
        );
        assert_eq!(r.topology.wrappable, vec!["api.ok.com"]);
        assert_eq!(r.topology.unreachable.len(), 1);
        assert_eq!(r.topology.unreachable[0].host, "api.stdlib.com");
        assert_eq!(r.topology.excluded.len(), 1);
        assert_eq!(r.topology.excluded[0].host, "api.broker.com");
        assert!(r.topology.excluded[0].reason.contains("risk_gate.py"));
        assert_eq!(
            r.topology.excluded[0].kind, "dependency-averse",
            "#64: kind must be the dependency-averse category, not loopback or any other"
        );
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
            child_runtime: None,
            env_inheritance: "inherited".into(),
        });
        scan.subprocesses.push(SubprocessSighting {
            file: "backup.py".into(),
            line: 20,
            launcher: "subprocess.run".into(),
            command: "backup now".into(),
            argv: Some(vec!["backup".into(), "now".into()]),
            child_runtime: None,
            env_inheritance: "inherited".into(),
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
            None,
            empty_boundaries(),
            &[],
            &[],
            &[],
            "unverified",
            None,
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
            child_runtime: None,
            env_inheritance: "inherited".into(),
        });
        scan.subprocesses.push(SubprocessSighting {
            file: "legacy.py".into(),
            line: 8,
            launcher: "subprocess.Popen".into(),
            command: "etl run".into(),
            argv: Some(vec!["etl".into(), "run".into()]),
            child_runtime: None,
            env_inheritance: "inherited".into(),
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
            None,
            empty_boundaries(),
            &[],
            &[],
            &[],
            "unverified",
            None,
        );
        assert!(
            r.topology
                .external_processes
                .iter()
                .all(|p| p.covered_by.is_none()),
            "neither os.system nor subprocess.Popen is ever a match candidate"
        );
    }

    /// Poll v2: a `hand-rolled-poll` whose scanner sighting carries an SDK
    /// poll shape gets an APPLYABLE route-key `poll` block — the shape names
    /// the provider's operation-read route, which beats the LLM host map.
    /// Already-configured route keys are not proposed twice, and a poll with
    /// no SDK shape (a URL-literal loop) gets the updated action with no fix.
    #[test]
    fn hand_rolled_poll_with_an_sdk_shape_carries_a_route_key_fix() {
        use crate::scan::SimplificationSighting;
        let mut scan = ScanResult::default();
        scan.simplifications.push(SimplificationSighting {
            file: "render.py".into(),
            line: 8,
            kind: "hand-rolled-poll".into(),
            function: "poll_video_takes".into(),
            targets: vec!["llm:google-genai".into()],
            sdk_polls: vec!["operations.get".into()],
            interval_s: None,
            deadline_s: None,
        });
        let topology = Topology {
            excluded: vec![],
            external_processes: vec![],
            unreachable: vec![],
            wrappable: vec!["llm:google-genai".to_owned()],
        };
        let (findings, _) = simplification_findings(
            &scan,
            &topology,
            Some("[target.\"llm:google-genai\"]\ntimeout = \"120s\"\n"),
        );
        let poll = findings
            .iter()
            .find(|f| f.topic == "hand-rolled-poll")
            .unwrap();
        let fix = poll.fix.as_ref().expect("route-key proposal attached");
        assert!(
            fix.patch
                .contains("[target.\"POST *-aiplatform.googleapis.com/*:fetchPredictOperation\"]"),
            "{}",
            fix.patch
        );
        assert!(
            fix.patch
                .contains("[target.\"GET generativelanguage.googleapis.com/*/operations/*\"]"),
            "{}",
            fix.patch
        );
        assert!(
            fix.patch
                .contains("until = { field = \"done\", terminal = [true], absent = \"pending\" }"),
            "a running google.longrunning.Operation omits `done` entirely — \
             the proposal must say absence means pending, not just terminal \
             values, or the applied block never polls (#128): {}",
            fix.patch
        );
        assert!(
            poll.action.contains("apply the attached patch"),
            "{}",
            poll.action
        );
        assert!(
            !poll.action.contains("GET/HEAD"),
            "slice-1 caveat is gone: {}",
            poll.action
        );
        // Already-present route key → that block is not proposed twice. The
        // document also names the Gemini host, so BOTH surfaces are detected
        // and the surface filter keeps the other block in play — this case is
        // about dedupe, not about surface narrowing (which
        // `the_detected_surface_narrows_the_patch_the_finding_carries` owns).
        let present = "[target.\"POST *-aiplatform.googleapis.com/*:fetchPredictOperation\"]\ntimeout = \"30s\"\n\n[target.\"generativelanguage.googleapis.com\"]\ntimeout = \"30s\"\n";
        let (f2, _) = simplification_findings(&scan, &topology, Some(present));
        let fix2 = f2[0].fix.as_ref().unwrap();
        assert!(
            !fix2.patch.contains("+[target.\"POST *-aiplatform"),
            "{}",
            fix2.patch
        );
        assert!(
            fix2.patch.contains("+[target.\"GET generativelanguage"),
            "{}",
            fix2.patch
        );
        // A SECOND sighting of the same provider shape proposes the same two
        // blocks against the same base file, and only one such patch can
        // apply — so the first carries it and the second names it.
        scan.simplifications.push(SimplificationSighting {
            file: "render.py".into(),
            line: 42,
            kind: "hand-rolled-poll".into(),
            function: "poll_again".into(),
            targets: vec!["llm:google-genai".into()],
            sdk_polls: vec!["operations.get".into()],
            interval_s: None,
            deadline_s: None,
        });
        let (dedup, _) = simplification_findings(
            &scan,
            &topology,
            Some("[target.\"llm:google-genai\"]\ntimeout = \"120s\"\n"),
        );
        assert!(dedup[0].fix.is_some(), "first sighting carries the patch");
        assert!(
            dedup[1].fix.is_none(),
            "same key set → not proposed twice: {:?}",
            dedup[1].fix
        );
        // #107.3: the pointer is structured (`fix_ref`) and the prose names
        // the holder by file:line instead of relying on report ORDER.
        assert_eq!(dedup[0].fix_ref, None, "the holder points at nobody");
        assert_eq!(dedup[1].fix_ref.as_deref(), Some("render.py:8"));
        assert!(
            dedup[1].action.contains(
                "The route-key patch for this provider is attached to the `hand-rolled-poll` \
                 finding for render.py:8 (`fix_ref`)."
            ),
            "{}",
            dedup[1].action
        );
        assert!(
            !dedup[1].action.contains("apply the attached patch"),
            "{}",
            dedup[1].action
        );
        // An invalid (or absent) keel.toml has no base document to edit — the
        // policy finding's removal fix owns that file until it parses.
        let (invalid, _) = simplification_findings(&scan, &topology, None);
        assert!(invalid.iter().all(|f| f.fix.is_none()), "{invalid:?}");
        scan.simplifications.pop();
        // URL-literal poll (no SDK shape) → no fix, action still updated.
        scan.simplifications[0].sdk_polls.clear();
        let (f3, _) = simplification_findings(&scan, &topology, Some(""));
        assert!(f3[0].fix.is_none());
        assert!(
            !f3[0].action.contains("attached"),
            "no patch exists to point at: {}",
            f3[0].action
        );
    }

    /// A google-genai SDK-poll sighting, with `scan.targets` carrying the raw
    /// host so the surface inference has something to read. `host` picks which
    /// surface the fixture represents; pass `None` for a project whose Google
    /// host never appears as a literal (the Indeterminate case).
    fn sdk_poll_fixture(host: Option<&str>) -> (ScanResult, Topology) {
        use crate::scan::SimplificationSighting;
        let mut scan = ScanResult::default();
        scan.simplifications.push(SimplificationSighting {
            file: "render.py".into(),
            line: 8,
            kind: "hand-rolled-poll".into(),
            function: "poll_video_takes".into(),
            targets: vec!["llm:google-genai".into()],
            sdk_polls: vec!["operations.get".into()],
            interval_s: None,
            deadline_s: None,
        });
        let evidence = |class| TargetEvidence {
            class,
            sightings: [Sighting {
                file: "render.py".into(),
                line: 8,
            }]
            .into_iter()
            .collect(),
        };
        scan.targets
            .insert("llm:google-genai".to_owned(), evidence(TargetClass::Llm));
        if let Some(h) = host {
            scan.targets
                .insert(h.to_owned(), evidence(TargetClass::Host));
        }
        let topology = Topology {
            excluded: vec![],
            external_processes: vec![],
            unreachable: vec![],
            wrappable: vec!["llm:google-genai".to_owned()],
        };
        (scan, topology)
    }

    #[test]
    fn proposals_are_filtered_to_the_detected_surface() {
        let polls = vec!["operations.get".to_owned()];

        let vertex_only = route_key_proposals_for(
            "llm:google-genai",
            &polls,
            &[crate::surface::Surface::Vertex],
        );
        let keys: Vec<&str> = vertex_only.iter().map(|p| p.key).collect();
        assert_eq!(
            keys,
            vec!["POST *-aiplatform.googleapis.com/*:fetchPredictOperation"]
        );

        let gemini_only = route_key_proposals_for(
            "llm:google-genai",
            &polls,
            &[crate::surface::Surface::GeminiApi],
        );
        let keys: Vec<&str> = gemini_only.iter().map(|p| p.key).collect();
        assert_eq!(
            keys,
            vec!["GET generativelanguage.googleapis.com/*/operations/*"]
        );

        // Both detected, and nothing detected, each keep both blocks.
        for set in [
            vec![
                crate::surface::Surface::GeminiApi,
                crate::surface::Surface::Vertex,
            ],
            vec![],
        ] {
            assert_eq!(
                route_key_proposals_for("llm:google-genai", &polls, &set).len(),
                2,
                "set {set:?} should keep both"
            );
        }
    }

    #[test]
    fn non_google_proposals_are_never_filtered_by_surface() {
        let polls = vec!["batches.retrieve".to_owned()];
        // An OpenAI proposal must survive a Google surface verdict.
        let got = route_key_proposals_for("llm:openai", &polls, &[crate::surface::Surface::Vertex]);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].key, "GET api.openai.com/v1/batches/*");
    }

    #[test]
    fn the_delete_one_note_appears_only_when_the_surface_is_unknown() {
        let polls = vec!["operations.get".to_owned()];

        let unknown = route_key_proposals_for("llm:google-genai", &polls, &[]);
        assert!(
            unknown.iter().all(|p| p.note.contains("delete")),
            "unknown surface should hedge: {:?}",
            unknown.iter().map(|p| &p.note).collect::<Vec<_>>()
        );

        for set in [
            vec![crate::surface::Surface::Vertex],
            vec![
                crate::surface::Surface::GeminiApi,
                crate::surface::Surface::Vertex,
            ],
        ] {
            let known = route_key_proposals_for("llm:google-genai", &polls, &set);
            assert!(
                known.iter().all(|p| !p.note.contains("delete")),
                "known surface {set:?} must not tell the operator to delete anything: {:?}",
                known.iter().map(|p| &p.note).collect::<Vec<_>>()
            );
        }
    }

    /// The detected surface reaches the emitted patch, not just the helper:
    /// a project whose scan sighted a Vertex host gets the Vertex block alone,
    /// and the evidence travels back out beside the findings.
    #[test]
    fn the_detected_surface_narrows_the_patch_the_finding_carries() {
        let (scan, topology) = sdk_poll_fixture(Some("us-central1-aiplatform.googleapis.com"));
        let (findings, surfaces) = simplification_findings(
            &scan,
            &topology,
            Some("[target.\"llm:google-genai\"]\ntimeout = \"120s\"\n"),
        );
        assert_eq!(surfaces.detected, vec![crate::surface::Surface::Vertex]);
        let patch = &findings[0].fix.as_ref().expect("a patch is attached").patch;
        assert!(
            patch.contains("POST *-aiplatform.googleapis.com/*:fetchPredictOperation"),
            "{patch}"
        );
        assert!(
            !patch.contains("generativelanguage.googleapis.com"),
            "the Gemini API block belongs to the other surface: {patch}"
        );
        assert!(
            !patch.contains("delete if you use"),
            "the surface is known — there is nothing to delete: {patch}"
        );

        // No Google host literal anywhere: the surface is indeterminate and
        // doctor hedges with both blocks.
        let (scan, topology) = sdk_poll_fixture(None);
        let (findings, surfaces) = simplification_findings(
            &scan,
            &topology,
            Some("[target.\"llm:google-genai\"]\ntimeout = \"120s\"\n"),
        );
        assert!(surfaces.detected.is_empty());
        let patch = &findings[0].fix.as_ref().expect("a patch is attached").patch;
        assert!(patch.contains("*-aiplatform.googleapis.com"), "{patch}");
        assert!(
            patch.contains("generativelanguage.googleapis.com"),
            "{patch}"
        );
        assert!(patch.contains("delete if you use"), "{patch}");
    }

    /// #139, the whole point of this program: an operator who already adopted
    /// the Vertex route key with a pre-CCR-11 `poll` block has an INERT poll —
    /// a running `google.longrunning.Operation` omits `done`, so the block
    /// returns on attempt one. Doctor used to drop the proposal on the mere
    /// presence of the section. It must amend it instead.
    #[test]
    fn an_existing_route_key_missing_absent_is_amended_not_suppressed() {
        let (scan, topology) = sdk_poll_fixture(Some("us-central1-aiplatform.googleapis.com"));
        let present = "[target.\"POST *-aiplatform.googleapis.com/*:fetchPredictOperation\"]\n\
             timeout = \"30s\"\n\
             poll    = { interval = \"10s\", deadline = \"30m\", until = { field = \"done\", \
             terminal = [true] } }\n";
        let (f, _) = simplification_findings(&scan, &topology, Some(present));
        let fix = f[0].fix.as_ref().expect("an amend fix must be attached");

        assert!(
            fix.patch.contains("absent"),
            "the inert block must be amended to add absent: {}",
            fix.patch
        );
        assert!(
            !fix.patch.contains("+[target.\"POST *-aiplatform"),
            "must not append a duplicate section: {}",
            fix.patch
        );
        // The applied document must carry the key where the poll layer reads
        // it, not merely somewhere in the file.
        assert!(
            fix.new_text
                .contains("until = { field = \"done\", terminal = [true], absent = \"pending\" }"),
            "{}",
            fix.new_text
        );
        assert!(
            f[0].action.contains("returns on the FIRST response"),
            "the finding must say the block the operator has does not poll: {}",
            f[0].action
        );
    }

    /// The case the original dedupe rule was actually written for: a route key
    /// that is already configured CORRECTLY is left entirely alone.
    #[test]
    fn an_existing_route_key_that_already_has_absent_is_left_alone() {
        let (scan, topology) = sdk_poll_fixture(Some("us-central1-aiplatform.googleapis.com"));
        let present = "[target.\"POST *-aiplatform.googleapis.com/*:fetchPredictOperation\"]\n\
             timeout = \"30s\"\n\
             poll    = { interval = \"10s\", deadline = \"30m\", until = { field = \"done\", \
             terminal = [true], absent = \"pending\" } }\n";
        let (f, _) = simplification_findings(&scan, &topology, Some(present));
        // Only ADDED lines count: the route key appears in the patch's context
        // lines whenever anything else in the file moves.
        let touches_vertex = |patch: &str| {
            patch
                .lines()
                .any(|l| l.starts_with('+') && l.contains("aiplatform"))
        };
        assert!(
            !f.iter()
                .filter_map(|x| x.fix.as_ref())
                .any(|fix| touches_vertex(&fix.patch)),
            "a correctly-configured block must be left alone: {f:?}"
        );

        // A no-op `Set` writes an empty patch, so "no patch" alone would pass
        // even if the amend fired. Give the same sighting a SECOND route that
        // really is missing, and the prose has to stay honest about which of
        // the two the patch touches.
        let (mut scan, mut topology) =
            sdk_poll_fixture(Some("us-central1-aiplatform.googleapis.com"));
        scan.simplifications[0].targets.push("llm:openai".into());
        scan.simplifications[0]
            .sdk_polls
            .push("batches.retrieve".into());
        topology.wrappable.push("llm:openai".to_owned());
        let (f, _) = simplification_findings(&scan, &topology, Some(present));
        let fix = f[0].fix.as_ref().expect("the OpenAI block is still added");
        assert!(fix.patch.contains("api.openai.com"), "{}", fix.patch);
        assert!(
            !touches_vertex(&fix.patch),
            "the Vertex block is already correct: {}",
            fix.patch
        );
        assert!(
            !f[0].action.contains("returns on the FIRST response"),
            "nothing was amended — the prose must not claim one was: {}",
            f[0].action
        );
    }

    /// One patch can carry both kinds of op — an appended block for a route
    /// the project does not declare, and an amend to one it declares inertly.
    /// Both clauses appear, and each is true of the patch.
    #[test]
    fn one_patch_can_both_append_a_block_and_amend_an_inert_one() {
        let (mut scan, mut topology) =
            sdk_poll_fixture(Some("us-central1-aiplatform.googleapis.com"));
        scan.simplifications[0].targets.push("llm:openai".into());
        scan.simplifications[0]
            .sdk_polls
            .push("batches.retrieve".into());
        topology.wrappable.push("llm:openai".to_owned());
        let present = "[target.\"POST *-aiplatform.googleapis.com/*:fetchPredictOperation\"]\n\
             timeout = \"30s\"\n\
             poll    = { interval = \"10s\", deadline = \"30m\", until = { field = \"done\", \
             terminal = [true] } }\n";
        let (f, _) = simplification_findings(&scan, &topology, Some(present));
        let fix = f[0].fix.as_ref().expect("a patch is attached");
        assert!(
            fix.new_text.contains("absent = \"pending\""),
            "the inert Vertex block is amended: {}",
            fix.new_text
        );
        assert!(
            fix.new_text
                .contains("[target.\"GET api.openai.com/v1/batches/*\"]"),
            "the OpenAI block is appended: {}",
            fix.new_text
        );
        assert!(
            f[0].action.contains("it adds the route-key"),
            "{}",
            f[0].action
        );
        assert!(f[0].action.contains("It ALSO sets"), "{}", f[0].action);
    }

    /// A target section with no `poll` table at all is a different
    /// conversation: appending the block would duplicate the section, so
    /// doctor skips it — and SAYS it skipped it rather than going quiet.
    #[test]
    fn an_existing_route_key_with_no_poll_table_is_named_not_duplicated() {
        let (scan, topology) = sdk_poll_fixture(Some("us-central1-aiplatform.googleapis.com"));
        let present = "[target.\"POST *-aiplatform.googleapis.com/*:fetchPredictOperation\"]\n\
                       timeout = \"30s\"\n";
        let (f, _) = simplification_findings(&scan, &topology, Some(present));
        assert!(
            f[0].fix.is_none(),
            "nothing to propose for this route: {:?}",
            f[0].fix
        );
        assert!(
            f[0].action.contains("with no `poll` table"),
            "the skip must be stated: {}",
            f[0].action
        );
    }

    /// Declaring the Vertex route key is itself Vertex evidence (`policy_hosts`
    /// reads the key), so the Gemini proposal is gone — #139's second half.
    #[test]
    fn declaring_the_vertex_route_key_stops_the_gemini_proposal() {
        let (scan, topology) = sdk_poll_fixture(Some("us-central1-aiplatform.googleapis.com"));
        let present = "[target.\"POST *-aiplatform.googleapis.com/*:fetchPredictOperation\"]\n\
             timeout = \"30s\"\n\
             poll    = { interval = \"10s\", deadline = \"30m\", until = { field = \"done\", \
             terminal = [true] } }\n";
        let (f, _) = simplification_findings(&scan, &topology, Some(present));
        for fix in f.iter().filter_map(|x| x.fix.as_ref()) {
            assert!(
                !fix.patch.contains("generativelanguage"),
                "a Vertex project must not be handed a Gemini route: {}",
                fix.patch
            );
        }
    }

    #[test]
    fn a_gemini_only_project_is_offered_only_the_gemini_route() {
        let (scan, topology) = sdk_poll_fixture(Some("generativelanguage.googleapis.com"));
        let (f, _) = simplification_findings(
            &scan,
            &topology,
            Some("[target.\"llm:google-genai\"]\ntimeout = \"120s\"\n"),
        );
        let fix = f[0].fix.as_ref().expect("a proposal must be attached");
        assert!(fix.patch.contains("generativelanguage"), "{}", fix.patch);
        assert!(!fix.patch.contains("aiplatform"), "{}", fix.patch);
    }

    #[test]
    fn a_project_using_both_surfaces_is_offered_both_without_a_delete_hint() {
        let (mut scan, topology) = sdk_poll_fixture(Some("us-central1-aiplatform.googleapis.com"));
        scan.targets.insert(
            "generativelanguage.googleapis.com".to_owned(),
            TargetEvidence {
                class: TargetClass::Host,
                sightings: [Sighting {
                    file: "render.py".into(),
                    line: 8,
                }]
                .into_iter()
                .collect(),
            },
        );
        let (f, _) = simplification_findings(
            &scan,
            &topology,
            Some("[target.\"llm:google-genai\"]\ntimeout = \"120s\"\n"),
        );
        let fix = f[0].fix.as_ref().expect("a proposal must be attached");
        assert!(fix.patch.contains("generativelanguage"), "{}", fix.patch);
        assert!(fix.patch.contains("aiplatform"), "{}", fix.patch);
        assert!(
            !fix.patch.contains("delete"),
            "both surfaces are in use — nothing to delete: {}",
            fix.patch
        );
    }

    /// #107.2: the proposed `poll` block states the loop's OWN cadence when
    /// the scanner could read it, and falls back to the documented defaults
    /// (`10s` / `30m`) when it could not — a sighting that knows nothing must
    /// not have a number invented for it.
    #[test]
    fn route_key_proposal_uses_the_loops_own_interval_and_deadline() {
        use crate::scan::SimplificationSighting;
        let sighting = |interval_s, deadline_s| SimplificationSighting {
            file: "render.py".into(),
            line: 8,
            kind: "hand-rolled-poll".into(),
            function: "poll_video_takes".into(),
            targets: vec!["llm:openai".into()],
            sdk_polls: vec!["batches.retrieve".into()],
            interval_s,
            deadline_s,
        };
        let topology = Topology {
            excluded: vec![],
            external_processes: vec![],
            unreachable: vec![],
            wrappable: vec!["llm:openai".to_owned()],
        };
        let patch_for = |s: SimplificationSighting| {
            let mut scan = ScanResult::default();
            scan.simplifications.push(s);
            simplification_findings(&scan, &topology, Some("[flows]\n")).0[0]
                .fix
                .as_ref()
                .expect("route-key proposal attached")
                .patch
                .clone()
        };
        let own = patch_for(sighting(Some(20), Some(900)));
        assert!(
            own.contains("interval = \"20s\", deadline = \"900s\""),
            "{own}"
        );
        // Partially known: the half Keel read is used, the half it did not
        // falls back — the two are independent.
        let half = patch_for(sighting(Some(20), None));
        assert!(
            half.contains("interval = \"20s\", deadline = \"30m\""),
            "{half}"
        );
        let neither = patch_for(sighting(None, None));
        assert!(
            neither.contains("interval = \"10s\", deadline = \"30m\""),
            "{neither}"
        );
    }

    /// #107.2, multi-sighting: ONE patch governs a route key that ALL the
    /// loops sharing it travel, so its cadence must be the most conservative
    /// of them — the MAXIMUM interval and the MAXIMUM deadline — never one
    /// member's numbers imposed on the rest. A single `None` in either column
    /// drops that column to its documented default; the two are independent.
    #[test]
    fn route_key_cadence_is_the_most_conservative_across_the_key_set() {
        use crate::scan::SimplificationSighting;
        let sighting = |line, interval_s, deadline_s| SimplificationSighting {
            file: "render.py".into(),
            line,
            kind: "hand-rolled-poll".into(),
            function: "poll".into(),
            targets: vec!["llm:openai".into()],
            sdk_polls: vec!["batches.retrieve".into()],
            interval_s,
            deadline_s,
        };
        let topology = Topology {
            excluded: vec![],
            external_processes: vec![],
            unreachable: vec![],
            wrappable: vec!["llm:openai".to_owned()],
        };
        let patch_for = |sightings: Vec<SimplificationSighting>| {
            let scan = ScanResult {
                simplifications: sightings,
                ..ScanResult::default()
            };
            let (findings, _) = simplification_findings(&scan, &topology, Some("[flows]\n"));
            findings
                .iter()
                .find_map(|f| f.fix.as_ref())
                .expect("route-key proposal attached")
                .patch
                .clone()
        };
        // Two loops on one route: the slower cadence wins both columns, so
        // the patch never speeds up the 3s loop to the 1s loop's pace.
        let both = patch_for(vec![
            sighting(8, Some(1), Some(120)),
            sighting(40, Some(3), Some(600)),
        ]);
        assert!(
            both.contains("interval = \"3s\", deadline = \"600s\""),
            "{both}"
        );
        // A third loop whose interval Keel could not read: the interval falls
        // back to the default while the deadline stays the maximum — the two
        // reasons are visibly distinct in one patch.
        let partial = patch_for(vec![
            sighting(8, Some(1), Some(120)),
            sighting(40, Some(3), Some(600)),
            sighting(70, None, Some(300)),
        ]);
        assert!(
            partial.contains("interval = \"10s\", deadline = \"600s\""),
            "{partial}"
        );
        // Provenance: a multi-loop patch must not name one loop as if the
        // numbers were its own, and must claim "slowest of them" ONLY for a
        // column it actually derived — `partial`'s `interval = "10s"` is the
        // default, and saying it was observed would be a false statement
        // beside the number.
        assert!(
            both.contains("replaces 2 hand-rolled polls on this route")
                && both.contains("interval and deadline are the slowest of them"),
            "{both}"
        );
        assert!(
            partial.contains(
                "deadline is the slowest of them, interval is Keel's default \
                 (a loop on this route declares none)"
            ),
            "{partial}"
        );
        let neither = patch_for(vec![sighting(8, None, None), sighting(40, Some(3), None)]);
        assert!(
            neither.contains(
                "interval and deadline are Keel's defaults \
                 (a loop on this route declares neither)"
            ),
            "{neither}"
        );
        let interval_only = patch_for(vec![
            sighting(8, Some(3), None),
            sighting(40, Some(9), None),
        ]);
        assert!(
            interval_only.contains(
                "interval is the slowest of them, deadline is Keel's default \
                 (a loop on this route declares none)"
            ),
            "{interval_only}"
        );
        assert!(
            !patch_for(vec![sighting(8, Some(3), Some(600))])
                .contains("hand-rolled polls on this route"),
            "a single sighting keeps the singular provenance"
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
            sdk_polls: vec![],
            interval_s: None,
            deadline_s: None,
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
            sdk_polls: vec![],
            interval_s: None,
            deadline_s: None,
        });
        let r = build_report(
            &scan,
            &BTreeSet::new(),
            default_policy(),
            default_journal(),
            None,
            None,
            empty_boundaries(),
            &[],
            &[],
            &[],
            "unverified",
            None,
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
        assert_eq!(
            poll.action,
            "Wrap the target, then replace the loop with a `poll` policy — `poll.deadline` \
             bounds the whole loop, `timeout` bounds one attempt. A POST-shaped operation read \
             (Vertex `:fetch*Operation`) polls too: put `poll` on a route key \
             (`[target.\"POST *-aiplatform.googleapis.com/*:fetchPredictOperation\"]`), which \
             beats the LLM host map for that route."
        );
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
            child_runtime: None,
            env_inheritance: "inherited".into(),
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
        // An LRO-sized timeout (also rank 5) — proves the alphabetical
        // within-rank tie-break against `preexisting-resilience`.
        let mut policy = default_policy();
        policy.lro_timeouts = vec![("target.\"llm:google-genai\"".to_string(), 1_800_000)];

        let r = build_report(
            &scan,
            &BTreeSet::new(),
            policy,
            default_journal(),
            None,
            None,
            empty_boundaries(),
            &[],
            &[],
            &[],
            "unverified",
            None,
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
                (5, "sdk-client-timeout", "target.\"llm:google-genai\""),
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

    /// Issue #80: a policy-declared timeout past the LRO-sized threshold
    /// (>600s) surfaces as exactly one rank-5 `sdk-client-timeout` follow-up,
    /// subject = the policy path, detail carrying the value in seconds.
    #[test]
    fn lro_sized_timeout_emits_the_sdk_client_timeout_follow_up() {
        let scan = ScanResult::default();
        let mut policy = default_policy();
        policy.lro_timeouts = vec![("target.\"llm:google-genai\"".to_string(), 1_800_000)];
        let r = build_report(
            &scan,
            &BTreeSet::new(),
            policy,
            default_journal(),
            None,
            None,
            empty_boundaries(),
            &[],
            &[],
            &[],
            "unverified",
            None,
        );
        let hit: Vec<_> = r
            .follow_ups
            .iter()
            .filter(|f| f.code == "sdk-client-timeout")
            .collect();
        assert_eq!(hit.len(), 1);
        assert_eq!(hit[0].rank, 5);
        assert_eq!(hit[0].subject, "target.\"llm:google-genai\"");
        assert!(hit[0].detail.contains("1800s"));
    }

    /// Deployment-honesty slice, WS6/WS10: the `sdk-client-timeout` detail
    /// must name all three clocks (Keel's `timeout`, the SDK's own
    /// client-default deadline, and `poll.deadline`) and, since poll v2,
    /// point a POST-shaped operation read (Vertex's `:fetch*Operation`) at a
    /// route key rather than at the retired `cache = { mode = "off" }`
    /// workaround.
    #[test]
    fn sdk_client_timeout_names_the_three_clocks_and_the_post_poll_caveat() {
        let ups = build_follow_ups(
            &Topology {
                excluded: vec![],
                external_processes: vec![],
                unreachable: vec![],
                wrappable: vec![],
            },
            None,
            &ScanResult::default(),
            &[],
            &[("target.\"llm:google-genai\"".to_owned(), 1_800_000)],
        );
        let fu = ups.iter().find(|u| u.code == "sdk-client-timeout").unwrap();
        assert_eq!(
            fu.detail,
            "timeout = 1800s bounds ONE attempt of ONE call, and is beyond the client-default \
             deadline most SDKs enforce (often ~600s). Keel wraps the transport; it does not raise \
             the SDK's own deadline — the call site must also pass a timeout >= the Keel value, or \
             the SDK gives up first and Keel just sees a retryable timeout. For a submit-then-poll \
             API a `poll` policy (whose `deadline` bounds the WHOLE loop) replaces the loop; a \
             POST-shaped operation read (Vertex `:fetch*Operation`) takes it on a route key — see \
             `hand-rolled-poll` findings for an applyable patch."
        );
    }

    /// No `lro_timeouts` entries (the common case — `validate_policy` never
    /// populates it for sub-threshold timeouts) means no follow-up.
    #[test]
    fn sub_lro_timeouts_emit_no_sdk_client_timeout_follow_up() {
        let scan = ScanResult::default();
        let policy = default_policy(); // lro_timeouts: Vec::new()
        let r = build_report(
            &scan,
            &BTreeSet::new(),
            policy,
            default_journal(),
            None,
            None,
            empty_boundaries(),
            &[],
            &[],
            &[],
            "unverified",
            None,
        );
        assert!(!r.follow_ups.iter().any(|f| f.code == "sdk-client-timeout"));
    }

    /// No signals → the field is present and empty (agents can rely on the key).
    #[test]
    fn no_signals_means_empty_follow_ups() {
        use crate::scan::TransportClass;
        let scan = scan_with("api.vendor.com", TargetClass::Host, &["httpx"]);
        // httpx is a tracked transport for this host.
        let mut scan = scan;
        scan.host_transports
            .insert("api.vendor.com".into(), TransportClass::Tracked);
        let r = build_report(
            &scan,
            &BTreeSet::new(),
            default_policy(),
            default_journal(),
            None,
            None,
            empty_boundaries(),
            &[],
            &[],
            &[],
            "unverified",
            None,
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
            None,
            empty_boundaries(),
            &[],
            &[],
            &[],
            "unverified",
            None,
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

    /// #64: a statically-seen loopback host is demoted to `excluded` (a test
    /// server or local dependency, not a real target) — UNLESS runtime
    /// evidence (`wrapped_targets`) says otherwise, which still wins per the
    /// precedence documented on [`classify_topology`].
    #[test]
    #[allow(clippy::too_many_lines)] // fixture setup + the round-1/round-2 finding/follow-up
    // assertions this test now carries (#64) are the legitimate length, not new plumbing.
    fn loopback_hosts_are_excluded_unless_runtime_wrapped() {
        use crate::scan::TransportClass;
        let mut scan = ScanResult {
            files_scanned: 1,
            python_available: true,
            ..ScanResult::default()
        };
        scan.targets.insert(
            "127.0.0.1".into(),
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
        scan.host_transports
            .insert("127.0.0.1".into(), TransportClass::Tracked);

        let r = build_report(
            &scan,
            &BTreeSet::new(),
            default_policy(),
            default_journal(),
            None,
            None,
            empty_boundaries(),
            &[],
            &[],
            &[],
            "unverified",
            None,
        );
        assert!(
            !r.topology.wrappable.contains(&"127.0.0.1".to_owned()),
            "a statically-seen loopback host must not be wrappable: {:?}",
            r.topology
        );
        let entry = r
            .topology
            .excluded
            .iter()
            .find(|e| e.host == "127.0.0.1")
            .unwrap_or_else(|| panic!("127.0.0.1 must land in excluded: {:?}", r.topology));
        assert!(
            entry.reason.contains("local/loopback"),
            "reason: {}",
            entry.reason
        );
        assert_eq!(
            entry.kind, "local/loopback",
            "#64: kind must be the loopback category, not dependency-averse or any other"
        );
        // #64: the `keel doctor` finding and follow-up must carry
        // category-accurate topic/action/code — never the dependency-averse
        // `# keel: include` advice, which is meaningless for a loopback host.
        let finding = r
            .findings
            .iter()
            .find(|f| f.topic == "local-host-excluded")
            .unwrap_or_else(|| panic!("no local-host-excluded finding: {:?}", r.findings));
        assert_eq!(finding.level, "info");
        assert!(
            !finding.action.contains("keel: include"),
            "loopback action must not carry dependency-averse advice: {}",
            finding.action
        );
        assert!(
            finding.action.contains("run under keel"),
            "loopback action should point at runtime evidence: {}",
            finding.action
        );
        assert!(
            !r.findings
                .iter()
                .any(|f| f.topic == "dependency-averse-excluded"),
            "must not also carry the dependency-averse topic: {:?}",
            r.findings
        );
        let follow_up = r
            .follow_ups
            .iter()
            .find(|f| f.subject == "127.0.0.1")
            .unwrap_or_else(|| panic!("no follow-up for 127.0.0.1: {:?}", r.follow_ups));
        assert_eq!(follow_up.code, "local-host-excluded");
        assert_eq!(
            follow_up.rank, 4,
            "same confidence tier as dependency-averse-excluded"
        );

        // Runtime evidence wins: the same host, wrapped at runtime, stays
        // wrappable.
        let wrapped: BTreeSet<String> = ["127.0.0.1".to_owned()].into_iter().collect();
        let r = build_report(
            &scan,
            &wrapped,
            default_policy(),
            default_journal(),
            None,
            None,
            empty_boundaries(),
            &[],
            &[],
            &[],
            "unverified",
            None,
        );
        assert!(
            r.topology.wrappable.contains(&"127.0.0.1".to_owned()),
            "a runtime-wrapped loopback host must stay wrappable: {:?}",
            r.topology
        );
        assert!(
            !r.topology.excluded.iter().any(|e| e.host == "127.0.0.1"),
            "must not also land in excluded"
        );
    }

    /// #67: `host_from_url` now unwraps a bracketed IPv6 authority before the
    /// port split, so the scanner can extract a bare `::1` from `http://[::1]:PORT/…`.
    /// This pins that `classify_topology`'s existing loopback demotion (#64)
    /// already covers the resulting bare IPv6 string with no doctor.rs code
    /// change — `IpAddr::is_loopback` is true for `::1` just as it is for
    /// `127.0.0.1`.
    #[test]
    fn ipv6_loopback_host_is_excluded_as_local_loopback() {
        use crate::scan::TransportClass;
        let mut scan = ScanResult {
            files_scanned: 1,
            python_available: true,
            ..ScanResult::default()
        };
        scan.targets.insert(
            "::1".into(),
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
        scan.host_transports
            .insert("::1".into(), TransportClass::Tracked);

        let topology = classify_topology(&scan, &BTreeSet::new(), &BTreeMap::new());
        assert!(
            !topology.wrappable.contains(&"::1".to_owned()),
            "a statically-seen IPv6 loopback host must not be wrappable: {topology:?}"
        );
        let entry = topology
            .excluded
            .iter()
            .find(|e| e.host == "::1")
            .unwrap_or_else(|| panic!("::1 must land in excluded: {topology:?}"));
        assert_eq!(
            entry.kind, "local/loopback",
            "#67/#64: kind must be the loopback category, not dependency-averse or any other"
        );
        assert!(
            entry.reason.contains("local/loopback"),
            "reason: {}",
            entry.reason
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
                path: None,
                present: false,
                valid: true,
            },
            cmd_match: BTreeMap::new(),
            lro_timeouts: Vec::new(),
            fix: None,
            text: None,
            flows_configured: false,
        };
        let r = build_report(
            &scan,
            &BTreeSet::new(),
            policy,
            default_journal(),
            None,
            None,
            empty_boundaries(),
            &[],
            &[],
            &[],
            "unverified",
            None,
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
            None,
            empty_boundaries(),
            &[],
            &[],
            &[],
            "unverified",
            None,
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
                path: None,
                present: false,
                valid: true,
            },
            cmd_match: BTreeMap::new(),
            lro_timeouts: Vec::new(),
            fix: None,
            text: None,
            flows_configured: false,
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
            None,
            empty_boundaries(),
            &[],
            &[],
            &[],
            "unverified",
            None,
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
            None,
            empty_boundaries(),
            &[],
            &[],
            &[],
            "unverified",
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
            None,
            empty_boundaries(),
            &[],
            &[],
            &[],
            "unverified",
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
            None,
            empty_boundaries(),
            &[],
            &[],
            &[],
            "unverified",
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
                path: Some("keel.toml".to_owned()),
                present: true,
                valid: false,
            },
            cmd_match: BTreeMap::new(),
            lro_timeouts: Vec::new(),
            fix: None,
            text: None,
            flows_configured: false,
        };
        let r = build_report(
            &scan,
            &wrapped,
            policy,
            default_journal(),
            None,
            None,
            empty_boundaries(),
            &[],
            &[],
            &[],
            "unverified",
            None,
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
                path: Some("keel.toml".to_owned()),
                present: true,
                valid: true,
            },
            cmd_match: BTreeMap::new(),
            lro_timeouts: Vec::new(),
            fix: None,
            text: None,
            flows_configured: false,
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
            None,
            empty_boundaries(),
            &[],
            &[],
            &[],
            "unverified",
            None,
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

    // ---- config above cwd (issue #85) ----

    /// A parent directory carries a `keel.toml` this project subdirectory
    /// does not — the finding must name both paths and point at `KEEL_CWD`.
    #[test]
    fn config_above_cwd_finding_fires_when_a_parent_has_keel_toml() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("keel.toml"), "[target.\"x\"]\n").unwrap();
        let project = dir.path().join("sub").join("nested");
        std::fs::create_dir_all(&project).unwrap();

        let finding =
            config_above_cwd_finding(&project).expect("parent keel.toml should be flagged");
        assert_eq!(finding.level, "warn");
        assert_eq!(finding.topic, "config-above-cwd");
        let canonical_parent = std::fs::canonicalize(dir.path()).unwrap();
        let canonical_project = std::fs::canonicalize(&project).unwrap();
        assert!(
            finding
                .detail
                .contains(&canonical_parent.display().to_string()),
            "detail names the parent: {}",
            finding.detail
        );
        assert!(
            finding
                .detail
                .contains(&canonical_project.display().to_string()),
            "detail names the project: {}",
            finding.detail
        );
        assert_eq!(
            finding.action,
            format!(
                "Run keel doctor from {p} to report against that policy; for runtime \
                 activation, set KEEL_CWD={p}. keel doctor reads its own working directory \
                 only.",
                p = canonical_parent.display()
            ),
            "the action must not claim KEEL_CWD changes what doctor itself reads"
        );
    }

    /// `main.rs` hands every subcommand `Path::new(".")`. These two std facts
    /// are exactly why the walk must canonicalize FIRST — pinned here so a
    /// future "simplification" back to `project.parent()` fails loudly instead
    /// of silently switching the finding off in production (the
    /// `keel doctor --json` child-process pin lives in `tests/cli.rs`).
    #[test]
    fn a_relative_dot_project_path_has_no_walkable_parent_chain() {
        assert_eq!(Path::new(".").parent(), Some(Path::new("")));
        assert_eq!(Path::new("").parent(), None);
    }

    /// The project has its own `keel.toml` — even though a parent also has
    /// one, there is nothing to flag: this project's own config is what
    /// actually loads.
    #[test]
    fn config_above_cwd_finding_is_none_when_project_has_its_own_keel_toml() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("keel.toml"), "[target.\"x\"]\n").unwrap();
        let project = dir.path().join("sub");
        std::fs::create_dir(&project).unwrap();
        std::fs::write(project.join("keel.toml"), "[target.\"y\"]\n").unwrap();

        assert!(config_above_cwd_finding(&project).is_none());
    }

    /// No `keel.toml` anywhere within the walk bound: nothing to flag.
    #[test]
    fn config_above_cwd_finding_is_none_when_no_keel_toml_is_found() {
        let dir = tempfile::TempDir::new().unwrap();
        let project = dir.path().join("sub");
        std::fs::create_dir(&project).unwrap();

        assert!(config_above_cwd_finding(&project).is_none());
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

    /// Issue #80: `validate_policy` extracts an LRO-sized timeout (>600s)
    /// from `[target."…"]`, `[defaults.llm]`, and `[defaults.outbound]` —
    /// but 600s exactly is NOT over the threshold (strictly greater-than).
    #[test]
    fn validate_policy_extracts_lro_sized_timeouts_strictly_over_threshold() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("keel.toml");
        std::fs::write(
            &path,
            "[target.\"llm:google-genai\"]\ntimeout = \"601s\"\n\n\
             [defaults.llm]\ntimeout = \"600s\"\n\n\
             [defaults.outbound]\ntimeout = \"900s\"\n",
        )
        .unwrap();
        let v = validate_policy(&path);
        assert!(v.check.valid);
        assert_eq!(
            v.lro_timeouts,
            vec![
                ("defaults.outbound".to_string(), 900_000),
                ("target.\"llm:google-genai\"".to_string(), 601_000),
            ]
        );
    }

    /// A `timeout` of exactly 600s (and anything under it) never extracts —
    /// the threshold is strictly greater-than.
    #[test]
    fn validate_policy_extracts_no_lro_timeouts_at_or_under_threshold() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("keel.toml");
        std::fs::write(&path, "[target.\"api.example.com\"]\ntimeout = \"600s\"\n").unwrap();
        let v = validate_policy(&path);
        assert!(v.check.valid);
        assert!(v.lro_timeouts.is_empty());
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
U = "https://api.leak.vendor.com/v1?key=CANARY_QUERY_9f31"

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
             G = \"https://api.gateonly.vendor.com/v2?tok=CANARY_QUERY2_9f31\"\n",
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
        assert!(doctor_json.contains("api.leak.vendor.com"));
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
            None,
            empty_boundaries(),
            &[],
            &[],
            &[],
            "unverified",
            None,
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

    /// Deployment-honesty slice, WS6/WS10: the keel skill's evaluation
    /// protocol grew a sixth phase ("Ship") — this string must say so.
    #[test]
    fn protocol_string_has_six_phases() {
        let b = boundaries(Path::new("."));
        assert!(b.protocol.contains("six phases"), "{}", b.protocol);
        assert!(b.protocol.contains("-> Ship"), "{}", b.protocol);
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
            None,
            boundaries(dir.path()),
            &[],
            &[],
            &[],
            "unverified",
            None,
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
            None,
            empty_boundaries(),
            &[],
            &[],
            &[],
            "unverified",
            None,
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
            None,
            empty_boundaries(),
            &[],
            &[],
            &[],
            "unverified",
            None,
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
            None,
            empty_boundaries(),
            &[],
            &[],
            &[],
            "unverified",
            None,
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
            None,
            empty_boundaries(),
            &[],
            &[],
            &[],
            "unverified",
            None,
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

    #[test]
    fn reserved_names_are_excluded_not_warned() {
        for host in [
            "example.com",
            "api.example.com",
            "e.com.example",
            "n.example",
            "web.test",
            "x.invalid",
            "192.0.2.10",
            "203.0.113.7",
            "2001:db8::1",
        ] {
            let mut scan = scan_with(host, TargetClass::Host, &["httpx"]);
            scan.host_transports
                .insert(host.to_owned(), TransportClass::Tracked);
            let r = build_report(
                &scan,
                &BTreeSet::new(),
                default_policy(),
                default_journal(),
                None,
                None,
                empty_boundaries(),
                &[],
                &[],
                &[],
                "unverified",
                None,
            );
            let entry = r
                .topology
                .excluded
                .iter()
                .find(|e| e.host == host)
                .unwrap_or_else(|| panic!("{host} must be excluded: {:?}", r.topology));
            assert_eq!(entry.kind, "reserved-name");
            assert!(
                !r.findings.iter().any(|f| f.topic == "visible-unwrapped"),
                "{host}: no duplicate warn"
            );
            assert!(
                r.findings
                    .iter()
                    .any(|f| f.topic == "reserved-name-excluded" && f.level == "info")
            );
            let fu = r.follow_ups.iter().find(|f| f.subject == host).unwrap();
            assert_eq!((fu.code, fu.rank), ("reserved-name-excluded", 4));
        }
        // Real hosts are untouched.
        assert!(!reserved_name("api.stripe.com"));
        assert!(!reserved_name("example.company.com"));
        assert!(!reserved_name("testing.internal"));
    }

    #[test]
    fn hosts_seen_only_in_test_files_are_excluded() {
        let mut scan = ScanResult {
            files_scanned: 2,
            python_available: true,
            ..ScanResult::default()
        };
        scan.targets.insert(
            "api.vendor.com".into(),
            TargetEvidence {
                class: TargetClass::Host,
                sightings: [
                    Sighting {
                        file: "tests/test_client.py".into(),
                        line: 3,
                    },
                    Sighting {
                        file: "tests/conftest.py".into(),
                        line: 9,
                    },
                ]
                .into_iter()
                .collect(),
            },
        );
        scan.host_transports
            .insert("api.vendor.com".into(), TransportClass::Tracked);
        let r = build_report(
            &scan,
            &BTreeSet::new(),
            default_policy(),
            default_journal(),
            None,
            None,
            empty_boundaries(),
            &[],
            &[],
            &[],
            "unverified",
            None,
        );
        let e = r
            .topology
            .excluded
            .iter()
            .find(|e| e.host == "api.vendor.com")
            .expect("excluded");
        assert_eq!(e.kind, "test-only");
        assert!(e.reason.contains("tests/conftest.py"));
        // One production sighting flips it back to wrappable.
        scan.targets
            .get_mut("api.vendor.com")
            .unwrap()
            .sightings
            .insert(Sighting {
                file: "app/client.py".into(),
                line: 1,
            });
        let r2 = build_report(
            &scan,
            &BTreeSet::new(),
            default_policy(),
            default_journal(),
            None,
            None,
            empty_boundaries(),
            &[],
            &[],
            &[],
            "unverified",
            None,
        );
        assert!(r2.topology.wrappable.contains(&"api.vendor.com".to_owned()));
    }

    #[test]
    fn runtime_evidence_beats_test_only_and_reserved_exclusion() {
        let mut scan = scan_with("example.com", TargetClass::Host, &["httpx"]);
        scan.host_transports
            .insert("example.com".into(), TransportClass::Tracked);
        let wrapped: BTreeSet<String> = ["example.com".to_owned()].into_iter().collect();
        let r = build_report(
            &scan,
            &wrapped,
            default_policy(),
            default_journal(),
            None,
            None,
            empty_boundaries(),
            &[],
            &[],
            &[],
            "unverified",
            None,
        );
        assert!(r.topology.wrappable.contains(&"example.com".to_owned()));
        assert!(r.topology.excluded.is_empty());

        // The other half of the name: a host sighted ONLY in test files, which
        // `classify_topology` would otherwise exclude as `test-only`. Runtime
        // evidence has to beat that check too — it sits below the
        // `wrapped_targets` shortcut, and this pins that ordering.
        let mut test_only = ScanResult {
            files_scanned: 1,
            python_available: true,
            ..ScanResult::default()
        };
        test_only.targets.insert(
            "api.vendor.com".into(),
            TargetEvidence {
                class: TargetClass::Host,
                sightings: [Sighting {
                    file: "tests/test_client.py".into(),
                    line: 3,
                }]
                .into_iter()
                .collect(),
            },
        );
        test_only
            .host_transports
            .insert("api.vendor.com".into(), TransportClass::Tracked);
        let observed: BTreeSet<String> = ["api.vendor.com".to_owned()].into_iter().collect();
        let r2 = build_report(
            &test_only,
            &observed,
            default_policy(),
            default_journal(),
            None,
            None,
            empty_boundaries(),
            &[],
            &[],
            &[],
            "unverified",
            None,
        );
        assert!(r2.topology.wrappable.contains(&"api.vendor.com".to_owned()));
        assert!(r2.topology.excluded.is_empty(), "{:?}", r2.topology);
    }

    #[test]
    fn loopback_no_longer_gets_a_duplicate_visible_unwrapped_warn() {
        let mut scan = scan_with("127.0.0.1", TargetClass::Host, &["httpx"]);
        scan.host_transports
            .insert("127.0.0.1".into(), TransportClass::Tracked);
        let r = build_report(
            &scan,
            &BTreeSet::new(),
            default_policy(),
            default_journal(),
            None,
            None,
            empty_boundaries(),
            &[],
            &[],
            &[],
            "unverified",
            None,
        );
        assert!(!r.findings.iter().any(|f| f.topic == "visible-unwrapped"));
        assert_eq!(
            r.coverage.visible_unwrapped,
            vec!["127.0.0.1".to_owned()],
            "the raw coverage list is unchanged"
        );
    }

    #[test]
    fn subprocess_sightings_in_test_files_are_counted_separately() {
        use crate::scan::SubprocessSighting;
        let mut scan = ScanResult {
            files_scanned: 2,
            python_available: true,
            ..ScanResult::default()
        };
        scan.subprocesses = vec![
            SubprocessSighting {
                file: "services/render.py".into(),
                line: 493,
                launcher: "subprocess.run".into(),
                command: "ffmpeg -i in.mp4".into(),
                argv: None,
                child_runtime: None,
                env_inheritance: "inherited".into(),
            },
            SubprocessSighting {
                file: "tests/test_stitch.py".into(),
                line: 12,
                launcher: "subprocess.run".into(),
                command: "ffmpeg -version".into(),
                argv: None,
                child_runtime: None,
                env_inheritance: "inherited".into(),
            },
        ];
        let r = build_report(
            &scan,
            &BTreeSet::new(),
            default_policy(),
            default_journal(),
            None,
            None,
            empty_boundaries(),
            &[],
            &[],
            &[],
            "unverified",
            None,
        );
        let procs = &r.topology.external_processes;
        assert_eq!(procs.iter().filter(|p| p.in_tests).count(), 1);
        let fu = r
            .follow_ups
            .iter()
            .find(|f| f.code == "subprocess-blind-spot")
            .unwrap();
        assert_eq!(
            fu.subject,
            "1 externally-launched process(es) (+1 in test files)"
        );
        assert!(fu.detail.contains("services/render.py:493"));
        assert!(!fu.detail.contains("tests/test_stitch.py"));
        let warn = r
            .findings
            .iter()
            .find(|f| f.topic == "subprocess-blind-spot" && f.level == "warn")
            .unwrap();
        assert!(!warn.detail.contains("tests/test_stitch.py"));
    }

    #[test]
    fn follow_up_closed_set_ranks_the_two_new_codes_at_four() {
        assert_eq!(follow_up_rank("reserved-name-excluded"), 4);
        assert_eq!(follow_up_rank("test-only-excluded"), 4);
        assert_eq!(
            excluded_kind_topic("reserved-name"),
            "reserved-name-excluded"
        );
        assert_eq!(excluded_kind_topic("test-only"), "test-only-excluded");
    }

    #[test]
    fn inherits_activation_is_derived_from_runtime_and_env() {
        use crate::scan::SubprocessSighting;
        let mk = |rt: Option<&str>, env: &str| SubprocessSighting {
            file: "a.py".into(),
            line: 1,
            launcher: "subprocess.run".into(),
            command: "x".into(),
            argv: None,
            child_runtime: rt.map(str::to_owned),
            env_inheritance: env.into(),
        };
        assert_eq!(
            inherits_activation(&mk(Some("python"), "inherited")),
            Some("python-pth")
        );
        assert_eq!(
            inherits_activation(&mk(Some("python"), "unknown")),
            Some("python-pth-if-env-passed")
        );
        assert_eq!(inherits_activation(&mk(Some("python"), "replaced")), None);
        assert_eq!(
            inherits_activation(&mk(Some("node"), "inherited")),
            Some("node-needs-NODE_OPTIONS")
        );
        assert_eq!(inherits_activation(&mk(None, "inherited")), None);
    }

    #[test]
    fn subprocess_follow_up_separates_inheriting_children_from_blind_spots() {
        use crate::scan::SubprocessSighting;
        let mut scan = ScanResult {
            files_scanned: 1,
            python_available: true,
            ..ScanResult::default()
        };
        scan.subprocesses = vec![
            SubprocessSighting {
                file: "agent.py".into(),
                line: 10,
                launcher: "subprocess.run".into(),
                command: "<dynamic>".into(),
                argv: None,
                child_runtime: Some("python".into()),
                env_inheritance: "inherited".into(),
            },
            SubprocessSighting {
                file: "stitch.py".into(),
                line: 20,
                launcher: "subprocess.run".into(),
                command: "ffmpeg -i in.mp4".into(),
                argv: Some(vec!["ffmpeg".into(), "-i".into(), "in.mp4".into()]),
                child_runtime: None,
                env_inheritance: "inherited".into(),
            },
        ];
        let r = build_report(
            &scan,
            &BTreeSet::new(),
            default_policy(),
            default_journal(),
            None,
            None,
            empty_boundaries(),
            &[],
            &[],
            &[],
            "unverified",
            None,
        );
        let fu = r
            .follow_ups
            .iter()
            .find(|f| f.code == "subprocess-blind-spot")
            .unwrap();
        assert_eq!(
            fu.subject,
            "1 externally-launched process(es) (+1 Python child that self-activates when it \
             inherits KEEL_ENABLE)"
        );
        assert!(
            fu.detail.contains("stitch.py:20") && !fu.detail.contains("agent.py:10"),
            "{}",
            fu.detail
        );
        let info = r
            .findings
            .iter()
            .find(|f| {
                f.topic == "subprocess-blind-spot"
                    && f.level == "info"
                    && f.detail.contains("agent.py:10")
            })
            .expect("an info finding for the inheriting child");
        assert!(info.detail.contains("self-activates"), "{}", info.detail);
    }

    #[test]
    fn inheriting_children_alone_still_produce_a_follow_up() {
        use crate::scan::SubprocessSighting;
        let mut scan = ScanResult {
            files_scanned: 1,
            python_available: true,
            ..ScanResult::default()
        };
        scan.subprocesses = vec![SubprocessSighting {
            file: "agent.py".into(),
            line: 10,
            launcher: "subprocess.run".into(),
            command: "<dynamic>".into(),
            argv: None,
            child_runtime: Some("python".into()),
            env_inheritance: "inherited".into(),
        }];
        let r = build_report(
            &scan,
            &BTreeSet::new(),
            default_policy(),
            default_journal(),
            None,
            None,
            empty_boundaries(),
            &[],
            &[],
            &[],
            "unverified",
            None,
        );
        let fu = r
            .follow_ups
            .iter()
            .find(|f| f.code == "subprocess-blind-spot")
            .expect(
                "#102: the count must be reachable from follow_ups, not only from the info finding",
            );
        assert!(
            fu.subject.contains("1 Python child that self-activates"),
            "{}",
            fu.subject
        );
        assert!(
            fu.detail.contains("no blind spot to chase"),
            "{}",
            fu.detail
        );
    }
}
