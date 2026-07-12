//! Integration + snapshot tests for the `keel` CLI.
//!
//! Snapshots are hand-rolled golden files under `tests/golden/`. Re-generate
//! them deliberately with `KEEL_UPDATE_GOLDEN=1 cargo test -p keel-cli`; without
//! that env var a mismatch fails the test (byte-for-byte). Determinism is the
//! whole point (dx-spec §5) — an agent diffs these to detect change.
//!
//! Fixture DBs are built the way the front ends build them: the journal from the
//! frozen `contracts/journal.sql` + the golden fixture inserts, the discovery
//! store through `keel-journal`'s own API.

use std::path::{Path, PathBuf};
use std::process::Command;

use keel_cli::render::json_string;
use keel_cli::{doctor, explain, init, scan, status};
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
    assert!(patch.starts_with("--- a/keel.toml\n+++ b/keel.toml\n"), "{patch}");

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
    assert!(patch.starts_with("--- /dev/null\n+++ b/keel.toml\n@@ -0,0 +1,"), "{patch}");

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
    let r = status::run(dir.path());
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

// ---- --json parity: every human-visible fact has a JSON counterpart ----

#[test]
fn status_json_parity_with_human() {
    let dir = tempfile::TempDir::new().unwrap();
    build_journal(dir.path());
    build_discovery(dir.path());
    let r = status::run(dir.path());

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
