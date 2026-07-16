//! Integration + snapshot tests for the `keel` CLI.
//!
//! Snapshots are hand-rolled golden files under `tests/golden/`. Re-generate
//! them deliberately with `KEEL_UPDATE_GOLDEN=1 cargo test -p keelrun-cli`; without
//! that env var a mismatch fails the test (byte-for-byte). Determinism is the
//! whole point (dx-spec §5) — an agent diffs these to detect change.
//!
//! Fixture DBs are built the way the front ends build them: the journal from the
//! frozen `contracts/journal.sql` + the golden fixture inserts, the discovery
//! store through `keel-journal`'s own API.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use keel_cli::render::json_string;
use keel_cli::{
    doctor, effective, explain, flows, flows_add, flows_suggest, init, replay, scan, status,
};
use keel_journal::{DiscoveryStore, ManualClock, TargetStats};

/// The completed/interrupted/dead flow fixtures (2026-07-11T00:00:00Z base).
const JOURNAL_SCHEMA: &str = include_str!("../../../contracts/journal.sql");
const COMPLETED_FLOW: &str =
    include_str!("../../../conformance/fixtures/journal/completed-flow.sql");
const INTERRUPTED_FLOW: &str =
    include_str!("../../../conformance/fixtures/journal/interrupted-flow.sql");
const DEAD_FLOW: &str = include_str!("../../../conformance/fixtures/journal/dead-flow.sql");

const T0: i64 = 1_783_728_000_000;

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn fixtures() -> PathBuf {
    manifest_dir().join("tests").join("fixtures")
}

fn golden_dir() -> PathBuf {
    manifest_dir().join("tests").join("golden")
}

/// Compare `actual` to the named golden file, or rewrite it under
/// `KEEL_UPDATE_GOLDEN`.
fn check_golden(name: &str, actual: &str) {
    let path = golden_dir().join(name);
    if std::env::var_os("KEEL_UPDATE_GOLDEN").is_some() {
        std::fs::write(&path, actual).expect("write golden");
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_default();
    assert_eq!(
        actual, expected,
        "golden mismatch for {name}; re-run with KEEL_UPDATE_GOLDEN=1 to update"
    );
}

fn python3_present() -> bool {
    Command::new("python3")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn node_present() -> bool {
    Command::new("node")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Run `cmd`, feed `stdin_data`, and return stdout. Panics on failure — the
/// caller has already checked the interpreter is present.
fn run_with_stdin(mut cmd: Command, stdin_data: &str) -> String {
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn subprocess");
    child
        .stdin
        .as_mut()
        .expect("child stdin")
        .write_all(stdin_data.as_bytes())
        .expect("write child stdin");
    let out = child.wait_with_output().expect("wait for subprocess");
    assert!(
        out.status.success(),
        "subprocess failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("subprocess stdout is UTF-8")
}

/// Build a `.keel/journal.db` at `project` with the three golden flows.
fn build_journal(project: &Path) {
    let keel = project.join(".keel");
    std::fs::create_dir_all(&keel).unwrap();
    let conn = rusqlite::Connection::open(keel.join("journal.db")).unwrap();
    conn.execute_batch(JOURNAL_SCHEMA).unwrap();
    conn.execute_batch(COMPLETED_FLOW).unwrap();
    conn.execute_batch(INTERRUPTED_FLOW).unwrap();
    conn.execute_batch(DEAD_FLOW).unwrap();
}

/// Build a `.keel/discovery.db` at `project` with two fixed target aggregates.
fn build_discovery(project: &Path) {
    let keel = project.join(".keel");
    std::fs::create_dir_all(&keel).unwrap();
    let store = DiscoveryStore::open(keel.join("discovery.db"), ManualClock::new(T0)).unwrap();
    store
        .merge_report(&[
            // Honors the discovery invariant calls == successes+failures+cache_hits.
            TargetStats {
                target: "api.example.com".to_owned(),
                calls: 100,
                attempts: 102,
                retries: 12,
                successes: 88,
                failures: 2,
                cache_hits: 10,
                throttled: 3,
                breaker_opens: 1,
                total_latency_ms: 12_000,
                max_latency_ms: 300,
                first_seen_ms: T0,
                last_seen_ms: T0 + 120_000,
                last_error_class: Some(keel_journal::ErrorClass::Http),
                last_error_status: Some(503),
                not_retried: 1,
                unwrapped_calls: 0,
            },
            TargetStats {
                target: "llm:openai".to_owned(),
                calls: 40,
                attempts: 20,
                retries: 0,
                successes: 20,
                failures: 0,
                cache_hits: 20,
                throttled: 0,
                breaker_opens: 0,
                total_latency_ms: 8_000,
                max_latency_ms: 400,
                first_seen_ms: T0,
                last_seen_ms: T0 + 60_000,
                last_error_class: None,
                last_error_status: None,
                not_retried: 0,
                unwrapped_calls: 5,
            },
        ])
        .unwrap();
}

// ---- init: two fixture mini-projects → byte-identical golden keel.toml ----

#[test]
fn init_node_fetch_matches_golden() {
    let scanned = scan::scan(&fixtures().join("node_fetch"));
    let out = init::render_keel_toml(&scanned, &[], None);
    check_golden("init_node.toml", &out);
}

#[test]
fn init_python_httpx_openai_matches_golden() {
    if !python3_present() {
        eprintln!("skip: python3 not available");
        return;
    }
    let scanned = scan::scan(&fixtures().join("py_httpx_openai"));
    let out = init::render_keel_toml(&scanned, &[], None);
    check_golden("init_py.toml", &out);
}

/// An `llm:*` target with observed discovery traffic gets an *active* rate limit
/// tuned from its evidence (dx-spec §1 flagship). Built from a hand-made scan +
/// discovery snapshot so it needs no python3 and stays byte-deterministic:
/// 200 calls over a 2-min window → mean 100/min → ×3 = 300 → clean 500/min.
#[test]
fn init_observed_llm_matches_golden() {
    let mut scanned = scan::ScanResult {
        files_scanned: 1,
        python_available: true,
        ..scan::ScanResult::default()
    };
    scanned.targets.insert(
        "llm:openai".to_owned(),
        scan::TargetEvidence {
            class: scan::TargetClass::Llm,
            sightings: [scan::Sighting {
                file: "agent.py".to_owned(),
                line: 12,
            }]
            .into_iter()
            .collect(),
        },
    );
    let discovery = vec![TargetStats {
        target: "llm:openai".to_owned(),
        calls: 200,
        attempts: 212,
        retries: 12,
        successes: 200,
        failures: 0,
        cache_hits: 0,
        throttled: 0,
        breaker_opens: 0,
        total_latency_ms: 40_000,
        max_latency_ms: 900,
        first_seen_ms: T0,
        last_seen_ms: T0 + 120_000,
        last_error_class: None,
        last_error_status: None,
        not_retried: 0,
        unwrapped_calls: 0,
    }];
    let out = init::render_keel_toml(&scanned, &discovery, None);
    check_golden("init_llm_observed.toml", &out);
}

/// `keel init --agents` drops a fixed, agent-facing section (dx-spec §5); its
/// bytes are golden so an agent can diff it across versions.
#[test]
fn init_agents_snippet_matches_golden() {
    check_golden("init_agents.md", &init::agents_block());
}

/// The packaged Claude Code Skill (`packaging/claude-skill/keel/SKILL.md`)
/// documents the six `keel mcp` tools by name for a different audience than
/// `AGENTS.md`'s snippet (an agent helping someone adopt/operate Keel from
/// outside, vs. one already working inside a Keel-adopted repo) — so the
/// prose is deliberately NOT shared, but the facts must not drift. This
/// guards the one fact most likely to silently rot: the tool name list,
/// cross-checked against `crate::mcp`'s own catalog rather than hardcoded
/// twice.
#[test]
fn skill_tool_list_matches_mcp_catalog() {
    const SKILL_MD: &str = include_str!("../../../packaging/claude-skill/keel/SKILL.md");
    const SKILLS_CHANNEL_MD: &str = include_str!("../../../skills/keel/SKILL.md");

    // skills/keel/SKILL.md must stay byte-identical to packaging/claude-skill/keel/SKILL.md —
    // edit one and copy to the other.
    assert_eq!(
        SKILL_MD, SKILLS_CHANNEL_MD,
        "skills/keel/SKILL.md must stay byte-identical to packaging/claude-skill/keel/SKILL.md — \
         edit one, copy to the other"
    );

    for name in keel_cli::mcp::TOOL_NAMES {
        assert!(
            SKILL_MD.contains(name),
            "packaging/claude-skill/keel/SKILL.md does not mention MCP tool `{name}` \
             (crate::mcp::TOOL_NAMES) — update the Skill's tool table"
        );
    }
}

// ---- init: agents-cli layout redirection ----

/// A real `keel init` run over the checked-in agents-cli fixture (manifest +
/// `app/` at the project root): the generated `keel.toml` lands inside `app/`,
/// not at the project root, and its bytes match what a plain `keel init`
/// would produce for that same tree — the redirection changes *where* the
/// file goes, never *what* gets written.
#[test]
fn init_writes_into_the_agent_dir_for_the_agents_cli_fixture() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::copy(
        fixtures()
            .join("agents_cli_project")
            .join("agents-cli-manifest.yaml"),
        dir.path().join("agents-cli-manifest.yaml"),
    )
    .unwrap();
    std::fs::create_dir(dir.path().join("app")).unwrap();
    std::fs::copy(
        fixtures()
            .join("agents_cli_project")
            .join("app")
            .join("app.mjs"),
        dir.path().join("app").join("app.mjs"),
    )
    .unwrap();

    let r = init::run(dir.path(), init::InitOptions::default());

    assert_eq!(r.exit, keel_cli::EXIT_OK);
    assert!(
        !dir.path().join("keel.toml").exists(),
        "no keel.toml left at the project root"
    );
    let written_path = dir.path().join("app").join("keel.toml");
    assert!(written_path.exists(), "keel.toml lands in app/");
    assert_eq!(
        r.json["wrote"].as_str().unwrap(),
        written_path.display().to_string()
    );

    // Same bytes a non-redirected `keel init` would have written for this
    // tree — redirection only changes the destination path.
    let scanned = scan::scan(dir.path());
    let expected = init::render_keel_toml(&scanned, &[], None);
    assert_eq!(std::fs::read_to_string(&written_path).unwrap(), expected);
}

// ---- init --diff: applyable policy diffs (dx-spec §5, lingua franca) ----

/// Two-target project for the `--diff` fixtures: `api.example.com` is already
/// in keel.toml (kept, untouched), `api.new.example` is new (added block).
const DIFF_APP_MJS: &str = "\
// two targets, one already in keel.toml
const KEPT = await fetch(\"https://api.example.com/v1/x\");
const ADDED = await fetch(\"https://api.new.example/v2/y\");
";

/// The pre-existing keel.toml: one kept target with user tuning + comments,
/// one stale target the scan no longer finds (removed block).
const DIFF_KEEL_TOML: &str = "\
# hand-tuned: keep this comment

[target.\"api.example.com\"]
timeout = \"9s\"   # user tuning survives

[target.\"api.gone.example\"]  # stale
timeout = \"5s\"
";

fn diff_project() -> tempfile::TempDir {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(dir.path().join("app.mjs"), DIFF_APP_MJS).unwrap();
    std::fs::write(dir.path().join("keel.toml"), DIFF_KEEL_TOML).unwrap();
    dir
}

fn init_diff(project: &Path) -> keel_cli::Rendered {
    let r = init::run(
        project,
        init::InitOptions {
            diff: true,
            stamp: false,
            agents: false,
        },
    );
    assert_eq!(r.exit, keel_cli::EXIT_OK);
    r
}

/// The whole `--json` twin of `keel init --diff` — summary, structured
/// `changes`, and the unified `patch` — is byte-golden (dx-spec §5).
#[test]
fn init_diff_json_matches_golden() {
    let dir = diff_project();
    let r = init_diff(dir.path());
    check_golden("init_diff.json", &json_string(&r.json));
}

fn git_present() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// The lingua-franca property, checked against the real tool: `git apply`
/// applies the emitted patch cleanly, the result parses to the proposed
/// policy, and every byte outside the touched blocks survives.
#[test]
fn init_diff_patch_applies_cleanly_with_git_apply() {
    if !git_present() {
        eprintln!("skip: git not available");
        return;
    }
    let dir = diff_project();
    let r = init_diff(dir.path());
    let patch = r.json["patch"].as_str().unwrap();
    assert!(
        patch.starts_with("--- a/keel.toml\n+++ b/keel.toml\n"),
        "{patch}"
    );

    std::fs::write(dir.path().join("keel.patch"), patch).unwrap();
    let out = Command::new("git")
        .args(["apply", "keel.patch"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git apply failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let applied = std::fs::read_to_string(dir.path().join("keel.toml")).unwrap();
    let value: toml::Value = applied.parse().expect("applied file parses");
    let targets = value["target"].as_table().unwrap();
    assert!(targets.contains_key("api.example.com"));
    assert!(targets.contains_key("api.new.example"));
    assert!(!targets.contains_key("api.gone.example"));
    // Untouched regions byte-preserved: header comment + user tuning.
    assert!(applied.contains("# hand-tuned: keep this comment"));
    assert!(applied.contains("timeout = \"9s\"   # user tuning survives"));
}

/// With no keel.toml, the patch is a `/dev/null` creation diff; `git apply`
/// creates a file byte-identical to what `keel init` itself would write.
#[test]
fn init_diff_creation_patch_matches_a_real_init_write() {
    if !git_present() {
        eprintln!("skip: git not available");
        return;
    }
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(dir.path().join("app.mjs"), DIFF_APP_MJS).unwrap();
    let r = init_diff(dir.path());
    let patch = r.json["patch"].as_str().unwrap();
    assert!(
        patch.starts_with("--- /dev/null\n+++ b/keel.toml\n@@ -0,0 +1,"),
        "{patch}"
    );

    std::fs::write(dir.path().join("keel.patch"), patch).unwrap();
    let out = Command::new("git")
        .args(["apply", "keel.patch"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git apply failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let created = std::fs::read_to_string(dir.path().join("keel.toml")).unwrap();
    let expected = init::render_keel_toml(&scan::scan(dir.path()), &[], None);
    assert_eq!(created, expected, "creation patch reproduces init's write");
}

// ---- status / doctor / explain: --json golden-tested ----

#[test]
fn status_json_matches_golden() {
    let dir = tempfile::TempDir::new().unwrap();
    build_journal(dir.path());
    build_discovery(dir.path());
    let r = status::run(dir.path(), T0);
    assert_eq!(r.exit, keel_cli::EXIT_OK);
    check_golden("status.json", &json_string(&r.json));
}

#[test]
fn doctor_json_matches_golden() {
    // Node fixture: JS scan is pure Rust (no python3). No discovery → the fetch
    // target is visible-but-unwrapped; a valid keel.toml keeps doctor ok.
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::copy(
        fixtures().join("node_fetch").join("app.mjs"),
        dir.path().join("app.mjs"),
    )
    .unwrap();
    std::fs::write(
        dir.path().join("keel.toml"),
        "[target.\"api.example.com\"]\nretry = { attempts = 5 }\n",
    )
    .unwrap();
    let r = doctor::run(dir.path());
    assert_eq!(r.exit, keel_cli::EXIT_OK);
    check_golden("doctor_node.json", &json_string(&r.json));
}

/// A project importing the six agent-framework packs plus google-adk/
/// google-genai: doctor's static scan classifies every one of them into
/// `findings.libs` (normalized to the REGISTRY names) with no "invisible"
/// coverage gap, since Task 4 registered adapters for all of them.
#[test]
fn doctor_json_matches_golden_for_agent_stack() {
    if !python3_present() {
        eprintln!("skip: python3 not available");
        return;
    }
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::copy(
        fixtures().join("py_agent_stack").join("app.py"),
        dir.path().join("app.py"),
    )
    .unwrap();
    let r = doctor::run(dir.path());
    assert_eq!(r.exit, keel_cli::EXIT_OK);
    check_golden("doctor_agent_stack.json", &json_string(&r.json));
}

/// An agents-cli project (a manifest naming `agent_directory: app`) with a
/// `keel.toml` left at the project root: the generated Dockerfile only COPYs
/// `pyproject.toml`, `README.md`, `uv.lock*`, and `app` into the image, so
/// doctor must flag the root file with an `agents-cli-config-placement`
/// warning. Built on the JS fixture (pure Rust scan, no python3) so the rest
/// of the report stays deterministic across machines.
#[test]
fn doctor_json_matches_golden_for_agents_cli_placement() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::copy(
        fixtures()
            .join("agents_cli_project")
            .join("agents-cli-manifest.yaml"),
        dir.path().join("agents-cli-manifest.yaml"),
    )
    .unwrap();
    std::fs::create_dir(dir.path().join("app")).unwrap();
    std::fs::copy(
        fixtures()
            .join("agents_cli_project")
            .join("app")
            .join("app.mjs"),
        dir.path().join("app").join("app.mjs"),
    )
    .unwrap();
    std::fs::write(
        dir.path().join("keel.toml"),
        "[target.\"api.example.com\"]\nretry = { attempts = 5 }\n",
    )
    .unwrap();

    let r = doctor::run(dir.path());
    assert_eq!(r.exit, keel_cli::EXIT_OK, "a warn finding does not fail ok");
    let findings = r.json["findings"].as_array().unwrap();
    assert!(
        findings
            .iter()
            .any(|f| f["topic"] == "agents-cli-config-placement" && f["level"] == "warn")
    );
    check_golden("doctor_agents_cli_placement.json", &json_string(&r.json));
}

/// An invalid keel.toml turns the doctor policy finding into an applyable fix
/// (dx-spec §5): the whole `--json` twin — findings, `fix.patch`,
/// `fix.changes` — is byte-golden, and the patch applies cleanly with the real
/// `git apply`, preserving every byte outside the removed entry.
#[test]
fn doctor_fix_json_matches_golden_and_applies() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("keel.toml"),
        "# my tuning\n[target.\"api.example.com\"]\ntimeout = \"30s\" # keep\nretry = { attempts = 0 }\n",
    )
    .unwrap();
    let r = doctor::run(dir.path());
    assert_eq!(r.exit, keel_cli::EXIT_USAGE, "invalid policy exits 2");
    assert!(r.human.contains("patch (apply with `git apply`)"));
    check_golden("doctor_fix.json", &json_string(&r.json));

    if !git_present() {
        eprintln!("skip: git not available");
        return;
    }
    let findings = r.json["findings"].as_array().unwrap();
    let fix = findings
        .iter()
        .find(|f| f["topic"] == "policy")
        .map(|f| &f["fix"])
        .expect("policy finding carries a fix");
    let patch = fix["patch"].as_str().unwrap();
    std::fs::write(dir.path().join("keel.patch"), patch).unwrap();
    let out = Command::new("git")
        .args(["apply", "keel.patch"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git apply failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let applied = std::fs::read_to_string(dir.path().join("keel.toml")).unwrap();
    assert!(applied.contains("# my tuning"));
    assert!(applied.contains("timeout = \"30s\" # keep"));
    assert!(!applied.contains("retry"), "invalid entry removed");
    // The fixed file passes a re-run: doctor is now ok.
    let again = doctor::run(dir.path());
    assert_eq!(
        again.exit,
        keel_cli::EXIT_OK,
        "removal fix heals the policy"
    );
}

/// The evidence readers honor `keel.toml`'s `journal` key: a journal at a
/// custom `file:` location (relative to the project) is found by `flows`,
/// `trace`, and `status` even though `.keel/journal.db` does not exist.
#[test]
fn flows_and_status_honor_the_policy_journal_location() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("keel.toml"),
        "journal = \"file:state/custom.db\"\n",
    )
    .unwrap();
    let state = dir.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    let conn = rusqlite::Connection::open(state.join("custom.db")).unwrap();
    conn.execute_batch(JOURNAL_SCHEMA).unwrap();
    conn.execute_batch(COMPLETED_FLOW).unwrap();
    drop(conn);

    let f = flows::flows(dir.path(), false, T0);
    assert_eq!(f.json["journal_present"], true, "custom journal found");
    assert_eq!(f.json["count"], 1, "the completed fixture flow is listed");

    let s = status::run(dir.path(), T0);
    assert_eq!(
        s.json["flows"]["total"], 1,
        "status reads the custom journal"
    );
}

// ---- flows suggest / flows add: the Level 2 on-ramp (dx-spec §1) ----

/// A JS project needs no interpreter to scan (pure Rust regex pass), so this
/// golden runs everywhere: one candidate, replay-safe, no discovery evidence.
#[test]
fn flows_suggest_json_matches_golden_for_a_js_project() {
    let r = flows_suggest::run(&fixtures().join("node_fetch"));
    assert_eq!(r.exit, keel_cli::EXIT_OK);
    check_golden("flows_suggest_node.json", &json_string(&r.json));
}

/// The full picture through the real scan + a real `.keel/discovery.db`: a
/// replay-safe candidate with idempotent-unsafe effects and virtualized
/// time/random reads (already designated in `keel.toml`), a replay-unsafe
/// candidate (subprocess use), and a pure helper that is not a candidate at
/// all. Exercises the discovery join (`FunctionFacts::targets` → observed
/// calls) end to end, not just the pure ranking unit test.
#[test]
fn flows_suggest_json_matches_golden_with_discovery_and_designation() {
    if !python3_present() {
        eprintln!("skip: python3 not available");
        return;
    }
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::copy(
        fixtures().join("py_flow_candidates").join("pipeline.py"),
        dir.path().join("pipeline.py"),
    )
    .unwrap();
    std::fs::write(
        dir.path().join("keel.toml"),
        "[flows]\nentrypoints = [\"py:pipeline:ingest\"]\n",
    )
    .unwrap();
    let keel_dir = dir.path().join(".keel");
    std::fs::create_dir_all(&keel_dir).unwrap();
    let store = DiscoveryStore::open(keel_dir.join("discovery.db"), ManualClock::new(T0)).unwrap();
    store
        .merge_report(&[TargetStats {
            target: "api.example.com".to_owned(),
            calls: 50,
            attempts: 50,
            retries: 0,
            successes: 50,
            failures: 0,
            cache_hits: 0,
            throttled: 0,
            breaker_opens: 0,
            total_latency_ms: 1_000,
            max_latency_ms: 50,
            first_seen_ms: T0,
            last_seen_ms: T0,
            last_error_class: None,
            last_error_status: None,
            not_retried: 0,
            unwrapped_calls: 0,
        }])
        .unwrap();

    let r = flows_suggest::run(dir.path());
    assert_eq!(r.exit, keel_cli::EXIT_OK);
    check_golden("flows_suggest_py.json", &json_string(&r.json));
}

/// `keel flows add`'s `--json` twin, both for a fresh `keel.toml` (the
/// `/dev/null`-headed creation patch) and for appending a second entrypoint —
/// the two shapes `keel init --diff` already golden-tests for policy edits in
/// general (dx-spec §5, diffs as the lingua franca).
#[test]
fn flows_add_json_matches_golden() {
    let dir = tempfile::TempDir::new().unwrap();
    let created = flows_add::run(dir.path(), "pipeline.ingest:main", false);
    assert_eq!(created.exit, keel_cli::EXIT_OK);
    check_golden("flows_add_create.json", &json_string(&created.json));

    let appended = flows_add::run(dir.path(), "jobs/nightly.ts#run", false);
    assert_eq!(appended.exit, keel_cli::EXIT_OK);
    check_golden("flows_add_append.json", &json_string(&appended.json));
}

/// The property `keel init --diff` is already golden-tested for: the emitted
/// patch applies cleanly with the real `git apply` and reproduces exactly what
/// a direct write would have produced.
#[test]
fn flows_add_patch_applies_cleanly_with_git_apply() {
    if !git_present() {
        eprintln!("skip: git not available");
        return;
    }
    let dir = tempfile::TempDir::new().unwrap();
    let r = flows_add::run(dir.path(), "pipeline.ingest:main", true);
    assert_eq!(r.exit, keel_cli::EXIT_OK);
    assert!(
        !dir.path().join("keel.toml").exists(),
        "--diff never writes"
    );
    let patch = r.json["patch"].as_str().unwrap();
    assert!(
        patch.starts_with("--- /dev/null\n+++ b/keel.toml\n"),
        "{patch}"
    );

    std::fs::write(dir.path().join("keel.patch"), patch).unwrap();
    let out = Command::new("git")
        .args(["apply", "keel.patch"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git apply failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let applied = std::fs::read_to_string(dir.path().join("keel.toml")).unwrap();
    // The applied file matches what a direct (non-diff) write for the same
    // entrypoint would have produced.
    let direct = tempfile::TempDir::new().unwrap();
    let w = flows_add::run(direct.path(), "pipeline.ingest:main", false);
    assert_eq!(w.exit, keel_cli::EXIT_OK);
    assert_eq!(
        applied,
        std::fs::read_to_string(direct.path().join("keel.toml")).unwrap(),
        "diff-then-apply reproduces a direct write"
    );
}

// ---- replay: the journal-driven dry run, --json golden over all three ----
// ---- golden flow shapes (completed / interrupted / dead)              ----

/// A completed flow re-enters as a pure replay: every step substitutes and the
/// whole `--json` plan is byte-golden.
#[test]
fn replay_completed_json_matches_golden() {
    let dir = tempfile::TempDir::new().unwrap();
    build_journal(dir.path());
    let r = replay::replay(dir.path(), "01JZWY0A0000000000000001", None);
    assert_eq!(r.exit, keel_cli::EXIT_OK);
    check_golden("replay_completed.json", &json_string(&r.json));
}

/// An interrupted flow resumes: steps 1–3 substitute, the crashed step 4
/// re-executes, and the cursor (`live_from_seq`) stands at 4.
#[test]
fn replay_interrupted_json_matches_golden() {
    let dir = tempfile::TempDir::new().unwrap();
    build_journal(dir.path());
    let r = replay::replay(dir.path(), "01JZWY0A0000000000000002", None);
    assert_eq!(r.exit, keel_cli::EXIT_OK);
    check_golden("replay_interrupted.json", &json_string(&r.json));
}

/// A dead flow is refused (KEEL-E032): the plan renders for inspection but no
/// step carries an action.
#[test]
fn replay_dead_json_matches_golden() {
    let dir = tempfile::TempDir::new().unwrap();
    build_journal(dir.path());
    let r = replay::replay(dir.path(), "01JZWY0A0000000000000003", None);
    assert_eq!(r.exit, keel_cli::EXIT_OK);
    check_golden("replay_dead.json", &json_string(&r.json));
}

/// `--step N` details one record, decoding its MessagePack payload; the
/// enrich step (seq 3, 2 attempts, `{"ok": true}`) is byte-golden.
#[test]
fn replay_step_detail_json_matches_golden() {
    let dir = tempfile::TempDir::new().unwrap();
    build_journal(dir.path());
    let r = replay::replay(dir.path(), "01JZWY0A0000000000000001", Some(3));
    assert_eq!(r.exit, keel_cli::EXIT_OK);
    check_golden("replay_step3.json", &json_string(&r.json));
}

/// The human plan is deterministic too (no wall-clock anywhere), so it can be
/// asserted directly: verdict, per-step actions, cursor.
#[test]
fn replay_human_plan_is_deterministic() {
    let dir = tempfile::TempDir::new().unwrap();
    build_journal(dir.path());
    let r = replay::replay(dir.path(), "01JZWY0A0000000000000002", None);
    let again = replay::replay(dir.path(), "01JZWY0A0000000000000002", None);
    assert_eq!(r.human, again.human);
    assert!(r.human.contains("dry run"));
    assert!(r.human.contains("\u{2192} substitute"));
    assert!(r.human.contains("\u{2192} re-execute"));
    assert!(r.human.contains("live execution resumes at seq 4"));
}

#[test]
fn explain_e014_json_matches_golden() {
    let r = explain::run("KEEL-E014");
    assert_eq!(r.exit, keel_cli::EXIT_OK);
    check_golden("explain_e014.json", &json_string(&r.json));
}

/// KEEL-E005 (unsupported-configuration, added by the defaults/E005 CCR) is the
/// code the flow gates raise; `keel explain` must carry its frozen copy.
#[test]
fn explain_e005_json_matches_golden() {
    let r = explain::run("KEEL-E005");
    assert_eq!(r.exit, keel_cli::EXIT_OK);
    check_golden("explain_e005.json", &json_string(&r.json));
}

// ---- doctor --effective-policy: golden + cross-language merge parity ----

/// The shared user policy the merge parity legs all consume: a wholesale llm
/// retry override, an outbound timeout override, and a pass-through target.
/// Matches `EFFECTIVE_KEEL_TOML` parsed to JSON.
const MERGE_FIXTURE: &str = r#"{
  "defaults": {
    "llm": { "retry": { "attempts": 2 } },
    "outbound": { "timeout": "10s" }
  },
  "target": {
    "api.example.com": { "retry": { "attempts": 5 } }
  }
}"#;

/// The same policy as the keel.toml the CLI-level golden test reads.
const EFFECTIVE_KEEL_TOML: &str = concat!(
    "[defaults.outbound]\n",
    "timeout = \"10s\"\n",
    "\n",
    "[defaults.llm]\n",
    "retry = { attempts = 2 }\n",
    "\n",
    "[target.\"api.example.com\"]\n",
    "retry = { attempts = 5 }\n",
);

/// The Rust merge of the shared fixture, as the canonical sorted-pretty JSON
/// bytes every implementation must reproduce.
fn rust_merge_json() -> String {
    let user: serde_json::Value = serde_json::from_str(MERGE_FIXTURE).unwrap();
    let merged = effective::effective_policy(&user, &[effective::llm_pack_fragment()]);
    json_string(&merged)
}

/// A fixture project whose JS scan detects the `openai` pack (pure Rust, no
/// python3) plus the shared keel.toml.
fn effective_fixture_project() -> tempfile::TempDir {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::copy(
        fixtures().join("node_openai").join("app.mjs"),
        dir.path().join("app.mjs"),
    )
    .unwrap();
    std::fs::write(dir.path().join("keel.toml"), EFFECTIVE_KEEL_TOML).unwrap();
    dir
}

#[test]
fn doctor_effective_json_matches_golden() {
    let dir = effective_fixture_project();
    let r = effective::run(dir.path());
    assert_eq!(r.exit, keel_cli::EXIT_OK);
    check_golden("doctor_effective.json", &json_string(&r.json));
}

#[test]
fn doctor_effective_human_matches_golden() {
    let dir = effective_fixture_project();
    let r = effective::run(dir.path());
    check_golden("doctor_effective.txt", &r.human);
}

#[test]
fn doctor_effective_level0_json_matches_golden() {
    // No keel.toml, no packs: the pure Level 0 composition.
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::copy(
        fixtures().join("node_fetch").join("app.mjs"),
        dir.path().join("app.mjs"),
    )
    .unwrap();
    let r = effective::run(dir.path());
    assert_eq!(r.exit, keel_cli::EXIT_OK);
    assert_eq!(r.json["user_policy_present"], serde_json::json!(false));
    check_golden("doctor_effective_level0.json", &json_string(&r.json));
}

/// The report's `policy` object IS the merge — byte-identical to the shared
/// merge golden the other two languages also reproduce.
#[test]
fn doctor_effective_policy_field_is_the_shared_merge() {
    let dir = effective_fixture_project();
    let r = effective::run(dir.path());
    assert_eq!(json_string(&r.json["policy"]), rust_merge_json());
}

#[test]
fn effective_merge_rust_matches_golden() {
    check_golden("effective_policy_merge.json", &rust_merge_json());
}

/// Python's `apply_pack_defaults` over the same fixture (with the provider
/// fragment its bootstrap would fold) must produce the same bytes.
#[test]
fn effective_merge_parity_python() {
    const SCRIPT: &str = r#"
import importlib.util, json, sys

spec = importlib.util.spec_from_file_location("keel_defaults", sys.argv[1])
mod = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mod)
user = json.loads(sys.stdin.read())
merged = mod.apply_pack_defaults(user, [{"defaults": {"llm": mod.llm_defaults()}}])
print(json.dumps(merged, sort_keys=True, indent=2))
"#;
    if !python3_present() {
        eprintln!("skip: python3 not available");
        return;
    }
    let defaults_py = manifest_dir().join("../../python/keel/src/keel/_defaults.py");
    let mut cmd = Command::new("python3");
    cmd.arg("-c").arg(SCRIPT).arg(defaults_py);
    let out = run_with_stdin(cmd, MERGE_FIXTURE);
    assert_eq!(out, rust_merge_json(), "Python merge diverges from Rust");
}

/// Node's `applyPackDefaults` over the same fixture must produce the same
/// bytes (it takes no fragments; the pack fold is identity by contract).
#[test]
fn effective_merge_parity_node() {
    const SCRIPT: &str = r#"
const { pathToFileURL } = require("node:url");
const sort = (v) =>
  Array.isArray(v)
    ? v.map(sort)
    : v && typeof v === "object"
      ? Object.fromEntries(Object.keys(v).sort().map((k) => [k, sort(v[k])]))
      : v;
let s = "";
process.stdin.on("data", (d) => (s += d));
process.stdin.on("end", async () => {
  const { applyPackDefaults } = await import(pathToFileURL(process.argv[1]).href);
  console.log(JSON.stringify(sort(applyPackDefaults(JSON.parse(s))), null, 2));
});
"#;
    if !node_present() {
        eprintln!("skip: node not available");
        return;
    }
    let defaults_mjs = manifest_dir().join("../../node/keel/src/defaults.mjs");
    let mut cmd = Command::new("node");
    cmd.arg("-e").arg(SCRIPT).arg(defaults_mjs);
    let out = run_with_stdin(cmd, MERGE_FIXTURE);
    assert_eq!(out, rust_merge_json(), "Node merge diverges from Rust");
}

// ---- --json parity: every human-visible fact has a JSON counterpart ----

#[test]
fn status_json_parity_with_human() {
    let dir = tempfile::TempDir::new().unwrap();
    build_journal(dir.path());
    build_discovery(dir.path());
    let r = status::run(dir.path(), T0);

    // Every top-level integer fact in the JSON twin must be shown to humans.
    for key in [
        "breaker_opens",
        "calls",
        "failures",
        "retries",
        "successes",
        "throttled",
    ] {
        let v = r.json[key].as_i64().unwrap();
        assert!(
            r.human.contains(&v.to_string()),
            "human output missing {key}={v}"
        );
    }
    // …and every flow count.
    for key in [
        "completed",
        "dead",
        "failed",
        "resumable",
        "running",
        "total",
    ] {
        let v = r.json["flows"][key].as_i64().unwrap();
        assert!(
            r.human.contains(&v.to_string()),
            "human output missing flows.{key}={v}"
        );
    }
    // targets_wrapped is a usize
    let tw = r.json["targets_wrapped"].as_u64().unwrap();
    assert!(r.human.contains(&tw.to_string()));
}

#[test]
fn doctor_json_parity_with_human() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::copy(
        fixtures().join("node_fetch").join("app.mjs"),
        dir.path().join("app.mjs"),
    )
    .unwrap();
    let r = doctor::run(dir.path());

    // Every adapter lib named in JSON appears in the human table, and vice versa.
    for adapter in r.json["adapters"].as_array().unwrap() {
        let lib = adapter["lib"].as_str().unwrap();
        assert!(r.human.contains(lib), "human output missing adapter {lib}");
    }
    // Coverage classes shown to humans.
    for target in r.json["coverage"]["visible_unwrapped"].as_array().unwrap() {
        assert!(r.human.contains(target.as_str().unwrap()));
    }
}
