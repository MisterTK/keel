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
use keel_journal::{Activation, DiscoveryStore, ManualClock, TargetStats};

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
    // `/code` is a literal, not the tempdir, so the golden stays stable
    // across machines (#92).
    store
        .record_activation(&Activation {
            ts_ms: T0,
            pid: 4242,
            language: "python".to_owned(),
            version: "0.5.6".to_owned(),
            cwd: "/code".to_owned(),
            keel_cwd: Some("/code".to_owned()),
            policy_source: "keel.toml".to_owned(),
            policy_path: Some("/code/keel.toml".to_owned()),
            flows_configured: true,
            argv0: "app.py".to_owned(),
            backend: Some("native".to_owned()),
        })
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

    // The Agent Skills spec caps `description` at 1024 characters and SILENTLY
    // TRUNCATES the overflow — a too-long description loses its tail, which is
    // where the "Do not use for…" anti-trigger clause lives. Pin the limit here
    // rather than discovering it as a skill that fires on everything.
    let description = SKILL_MD
        .lines()
        .find_map(|l| l.strip_prefix("description: "))
        .expect("SKILL.md frontmatter has a `description:` line");
    assert!(
        description.len() <= 1024,
        "skills/keel/SKILL.md description is {} characters; the spec hard limit is 1024 and \
         the overflow is silently truncated",
        description.len()
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

/// Two-target project for the `--diff` fixtures: `api.vendor.com` is already
/// in keel.toml (kept, untouched), `api.new-vendor.com` is new (added block).
/// Neither is an RFC 2606 reserved name — those are excluded as fixtures (WS5)
/// and would never be proposed.
const DIFF_APP_MJS: &str = "\
// two targets, one already in keel.toml
const KEPT = await fetch(\"https://api.vendor.com/v1/x\");
const ADDED = await fetch(\"https://api.new-vendor.com/v2/y\");
";

/// The pre-existing keel.toml: one kept target with user tuning + comments,
/// one stale target the scan no longer finds (removed block).
const DIFF_KEEL_TOML: &str = "\
# hand-tuned: keep this comment

[target.\"api.vendor.com\"]
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
    assert!(targets.contains_key("api.vendor.com"));
    assert!(targets.contains_key("api.new-vendor.com"));
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
    // Nothing Google-shaped anywhere in this project: the whole `llm_surfaces`
    // map is omitted, not reported as an empty object. The golden pins the
    // same fact; this says out loud that the absence is the assertion.
    assert!(
        r.json.get("llm_surfaces").is_none(),
        "a non-Google project carries no Google-shaped hole: {}",
        json_string(&r.json)
    );
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
        "[target.\"api.vendor.com\"]\nretry = { attempts = 5 }\n",
    )
    .unwrap();
    // A root CLAUDE.md so one golden pins `boundaries.governance_files`
    // non-empty; the other doctor goldens pin the empty case.
    std::fs::write(dir.path().join("CLAUDE.md"), "# project rules\n").unwrap();

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

/// Poll v2 (#93/#101): three SDK-shaped poll loops — the bound-result shape
/// and both inline `client.operations.get(op).done` shapes — each get a
/// `hand-rolled-poll` finding, and the LRO-sized timeout in the same fixture
/// pins the rewritten `sdk-client-timeout` text. All three would propose the
/// SAME two route keys, so exactly ONE carries the patch and the others point
/// at it. That patch applies with the real `git apply`, and a second run sees
/// both route keys present and proposes nothing at all.
#[test]
fn doctor_sdk_poll_route_key_fix_matches_golden_and_applies() {
    if !python3_present() {
        eprintln!("skip: python3 not available");
        return;
    }
    let dir = tempfile::TempDir::new().unwrap();
    for f in ["render.py", "inline.py", "keel.toml"] {
        std::fs::copy(fixtures().join("py_sdk_poll").join(f), dir.path().join(f)).unwrap();
    }
    let r = doctor::run(dir.path());
    assert_eq!(r.exit, keel_cli::EXIT_OK, "a poll lead does not flip ok");
    check_golden("doctor_sdk_poll_fix.json", &json_string(&r.json));

    if !git_present() {
        eprintln!("skip: git not available");
        return;
    }
    let findings = r.json["findings"].as_array().unwrap();
    let polls: Vec<_> = findings
        .iter()
        .filter(|f| f["topic"] == "hand-rolled-poll")
        .collect();
    assert_eq!(polls.len(), 3, "{}", json_string(&r.json));
    let with_fix: Vec<_> = polls.iter().filter(|f| !f["fix"].is_null()).collect();
    assert_eq!(
        with_fix.len(),
        1,
        "identical route keys → exactly one applyable patch: {}",
        json_string(&r.json)
    );
    // #107: the duplicates point at the holder by `file:line`, in a
    // structured field as well as in the prose — not by report position.
    for f in polls.iter().filter(|f| f["fix"].is_null()) {
        let r = f["fix_ref"].as_str().expect("structured fix_ref");
        assert_eq!(r, "inline.py:6", "{f}");
        assert!(
            f["action"].as_str().unwrap().contains(&format!(
                "attached to the `hand-rolled-poll` finding for {r} (`fix_ref`)"
            )),
            "{f}"
        );
    }
    assert!(
        with_fix[0]["fix_ref"].is_null(),
        "the holder points at nobody: {}",
        with_fix[0]
    );
    let patch = with_fix[0]["fix"]["patch"].as_str().unwrap();
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
    assert!(applied.contains("timeout = \"1800s\""), "{applied}");
    assert!(
        applied.contains("[target.\"POST *-aiplatform.googleapis.com/*:fetchPredictOperation\"]"),
        "{applied}"
    );
    let again = doctor::run(dir.path());
    assert_eq!(
        again.exit,
        keel_cli::EXIT_OK,
        "the proposed route keys validate: {}",
        json_string(&again.json)
    );
    for f in again.json["findings"].as_array().unwrap() {
        if f["topic"] == "hand-rolled-poll" {
            assert!(
                f["fix"].is_null(),
                "both route keys are configured — nothing left to propose: {f}"
            );
        }
    }
}

/// #139: a project that ALREADY adopted the Vertex route key, with a
/// pre-CCR-11 `poll` block that therefore never polls. Doctor used to drop the
/// proposal on the mere presence of the section and offer only the irrelevant
/// Gemini block. It must instead (a) narrow to Vertex — the declared route key
/// is itself the evidence — and (b) amend the inert block in place with
/// `until.absent = "pending"`, via a patch that really applies and re-validates.
#[test]
fn doctor_amends_an_inert_route_key_and_the_patch_applies() {
    if !python3_present() {
        eprintln!("skip: python3 not available");
        return;
    }
    let dir = tempfile::TempDir::new().unwrap();
    for f in ["render.py", "keel.toml"] {
        std::fs::copy(
            fixtures().join("py_sdk_poll_inert").join(f),
            dir.path().join(f),
        )
        .unwrap();
    }
    let r = doctor::run(dir.path());
    assert_eq!(r.exit, keel_cli::EXIT_OK, "a poll lead does not flip ok");
    // The verdict that narrowed the proposal is reported, not just acted on:
    // here the declared route key is itself the Vertex evidence.
    assert_eq!(
        r.json["llm_surfaces"]["llm:google-genai"],
        serde_json::json!({
            "detected": ["vertex"],
            "source": "policy",
            "evidence": ["*-aiplatform.googleapis.com"],
        }),
        "{}",
        json_string(&r.json)
    );
    assert!(
        r.human.contains("google surface: vertex"),
        "the human report says it too: {}",
        r.human
    );
    check_golden("doctor_sdk_poll_amend.json", &json_string(&r.json));

    if !git_present() {
        eprintln!("skip: git not available");
        return;
    }
    let poll = r.json["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["topic"] == "hand-rolled-poll")
        .expect("a hand-rolled-poll finding");
    let patch = poll["fix"]["patch"]
        .as_str()
        .expect("the inert block is amended, not suppressed");
    assert!(
        !patch.contains("generativelanguage"),
        "a Vertex project must not be handed a Gemini route: {patch}"
    );
    assert!(
        !patch.contains("+[target.\"POST *-aiplatform"),
        "the section exists — amend it, never duplicate it: {patch}"
    );
    assert!(
        poll["action"]
            .as_str()
            .unwrap()
            .contains("returns on the FIRST response"),
        "{poll}"
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
    assert!(applied.contains("absent = \"pending\""), "{applied}");
    assert!(
        applied.contains("# The route key an operator adopted before CCR-11"),
        "surgical edit: the operator's comments survive: {applied}"
    );
    let again = doctor::run(dir.path());
    assert_eq!(
        again.exit,
        keel_cli::EXIT_OK,
        "the amended route key validates: {}",
        json_string(&again.json)
    );
    for f in again.json["findings"].as_array().unwrap() {
        if f["topic"] == "hand-rolled-poll" {
            assert!(
                f["fix"].is_null(),
                "the route key now carries `absent` — nothing left to propose: {f}"
            );
        }
    }
}

// ---- config-above-cwd through the REAL binary (issue #85) ----

/// The built `keel` binary (the `CARGO_BIN_EXE_keel` convention `tests/exec.rs`
/// and `tests/flows_force.rs` already use).
fn keel_bin() -> &'static str {
    env!("CARGO_BIN_EXE_keel")
}

/// `keel doctor --json` as a real child process rooted at `cwd` — the only
/// faithful way to exercise the `project = Path::new(".")` that `main.rs`
/// passes every subcommand. Child-process cwd, never `std::env::set_current_dir`
/// (issue #72: process-global mutation is unsound against a parallel test
/// binary).
fn doctor_json_from(cwd: &Path) -> serde_json::Value {
    let out = Command::new(keel_bin())
        .current_dir(cwd)
        .arg("doctor")
        .arg("--json")
        .output()
        .expect("spawn keel doctor --json");
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "keel doctor --json emitted unparseable output ({e}): {}",
            String::from_utf8_lossy(&out.stdout)
        )
    })
}

fn has_topic(report: &serde_json::Value, topic: &str) -> bool {
    report["findings"]
        .as_array()
        .expect("findings array")
        .iter()
        .any(|f| f["topic"] == topic)
}

/// Regression pin for the production path of `config_above_cwd_finding`: run
/// from `root/sub/` (a `keel.toml` above, none here) the finding must fire, and
/// run from `root/` itself it must not. The in-crate unit tests pass an
/// ABSOLUTE project path and so never noticed that the relative `.` `main.rs`
/// actually passes made the parent walk terminate immediately — this test is
/// the only one that sees what an adopter sees.
///
/// Asserts on the finding's presence by topic, not on a byte-golden report:
/// every path in it is tempdir-specific.
#[test]
fn doctor_reports_config_above_cwd_when_run_from_a_subdirectory() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("keel.toml"),
        "[target.\"api.example.com\"]\n",
    )
    .unwrap();
    let sub = dir.path().join("sub");
    std::fs::create_dir(&sub).unwrap();

    let from_sub = doctor_json_from(&sub);
    assert!(
        has_topic(&from_sub, "config-above-cwd"),
        "running from a subdirectory of a keel.toml-bearing root must warn: {}",
        json_string(&from_sub)
    );

    let from_root = doctor_json_from(dir.path());
    assert!(
        !has_topic(&from_root, "config-above-cwd"),
        "running from the root that OWNS the keel.toml must not warn: {}",
        json_string(&from_root)
    );
}

/// WS3: a root Dockerfile whose COPY/ADD directives never reach keel.toml —
/// the exact shape of the 2026-09-15 field outage. Doctor must say so, and
/// `policy.path` must be machine-checkable.
#[test]
fn doctor_json_matches_golden_for_dockerfile_without_keel_toml() {
    if !python3_present() {
        eprintln!("skip: python3 not available");
        return;
    }
    let dir = tempfile::TempDir::new().unwrap();
    for f in ["app.py", "keel.toml", "Dockerfile"] {
        std::fs::copy(
            fixtures().join("py_dockerfile_no_copy").join(f),
            dir.path().join(f),
        )
        .unwrap();
    }
    let r = doctor::run(dir.path());
    assert_eq!(
        r.exit,
        keel_cli::EXIT_OK,
        "a packaging warning does not flip ok"
    );
    assert_eq!(r.json["policy"]["path"], "keel.toml");
    assert!(
        has_topic(&r.json, "keel-toml-not-in-image"),
        "{}",
        json_string(&r.json)
    );
    check_golden("doctor_dockerfile_no_copy.json", &json_string(&r.json));
}

/// #129: a native activation's `backend` must reach `keel doctor --json` —
/// `bootstrap.py`/`bootstrap.mjs` always folded `backend` into the row handed
/// to `record_activation`, but `_write_activation`'s fixed column tuple
/// silently dropped it, so doctor had no way to say which backend actually
/// ran even though the row otherwise verified. Builds `.keel/discovery.db`
/// directly through `keel-journal`'s own API (the same convention `Activation`
/// fixtures elsewhere in this file use) with a row whose policy identity
/// matches `project`, so `runtime_activation` is "verified" and
/// `activation_backend` is exercised, not left null by an unverified row.
#[test]
fn doctor_reports_the_activation_backend_from_a_verified_row() {
    let dir = tempfile::TempDir::new().unwrap();
    let project = dir.path();
    std::fs::write(project.join("keel.toml"), "").unwrap();
    let keel = project.join(".keel");
    std::fs::create_dir_all(&keel).unwrap();
    let store = DiscoveryStore::open(keel.join("discovery.db"), ManualClock::new(T0)).unwrap();
    store
        .record_activation(&Activation {
            ts_ms: T0,
            pid: 4242,
            language: "python".to_owned(),
            version: "0.7.0".to_owned(),
            cwd: project.to_string_lossy().into_owned(),
            keel_cwd: None,
            policy_source: "keel.toml".to_owned(),
            policy_path: Some(project.join("keel.toml").to_string_lossy().into_owned()),
            flows_configured: false,
            argv0: "app.py".to_owned(),
            backend: Some("native".to_owned()),
        })
        .unwrap();
    drop(store);

    let r = doctor::run(project);
    assert_eq!(r.json["runtime_activation"], "verified", "{}", r.json);
    assert_eq!(r.json["activation_backend"], "native", "{}", r.json);
}

/// A project with `[flows]` configured, a default SQLite journal, and a root
/// Dockerfile that DOES ship keel.toml (`COPY . /code`) — the exact case
/// `keel-toml-not-in-image` does NOT catch, since the artifact fully ships
/// the policy. Issue #90: durable-flow state on that same container's
/// filesystem does not survive an instance replacement either.
#[test]
fn doctor_json_matches_golden_for_flows_dockerfile() {
    if !python3_present() {
        eprintln!("skip: python3 not available");
        return;
    }
    let dir = tempfile::TempDir::new().unwrap();
    for f in ["app.py", "keel.toml", "Dockerfile"] {
        std::fs::copy(
            fixtures().join("py_flows_dockerfile").join(f),
            dir.path().join(f),
        )
        .unwrap();
    }
    let r = doctor::run(dir.path());
    assert_eq!(
        r.exit,
        keel_cli::EXIT_OK,
        "a packaging warning does not flip ok"
    );
    assert!(
        has_topic(&r.json, "journal-ephemeral-storage"),
        "{}",
        json_string(&r.json)
    );
    assert!(
        !has_topic(&r.json, "keel-toml-not-in-image"),
        "the Dockerfile ships keel.toml — should not ALSO warn about that: {}",
        json_string(&r.json)
    );
    check_golden("doctor_flows_dockerfile.json", &json_string(&r.json));
}

/// An agents-cli project root with `agent_directory: app`, `app/` present, and
/// a `keel.toml` written at `<root>/<toml_at>`. Returns the root TempDir.
fn agents_cli_tree(toml_at: &str) -> tempfile::TempDir {
    let dir = tempfile::TempDir::new().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("agents-cli-manifest.yaml"),
        "agent_directory: app\n",
    )
    .unwrap();
    std::fs::create_dir_all(root.join("app")).unwrap();
    let toml = root.join(toml_at);
    std::fs::create_dir_all(toml.parent().unwrap()).unwrap();
    std::fs::write(toml, "").unwrap();
    dir
}

/// The first finding with `topic`, or `None`.
fn finding_by_topic<'a>(
    report: &'a serde_json::Value,
    topic: &str,
) -> Option<&'a serde_json::Value> {
    report["findings"]
        .as_array()
        .expect("findings array")
        .iter()
        .find(|f| f["topic"] == topic)
}

/// #87: the agents-cli manifest walk must find a manifest ABOVE a relative
/// project path (`main.rs` passes "."), same defect class as #85's C1.
///
/// Run from `<root>/svc/sub` — two levels below the manifest, and OUTSIDE the
/// `app/` directory the generated Dockerfile ships — with a `keel.toml` right
/// there. The walk has to climb two levels for the finding to exist at all, so
/// its presence is the #87 regression proof. Its *text* is asserted too: at
/// level > 0 the layout carries canonical absolute paths, and the finding is
/// documented (`doctor.rs::agents_cli_placement_finding`) to stay reproducible
/// across checkouts — so nothing machine-specific may leak into `--json`.
#[test]
fn doctor_finds_the_agents_cli_manifest_from_a_nested_subdirectory() {
    let dir = agents_cli_tree("svc/sub/keel.toml");
    let nested = dir.path().join("svc").join("sub");
    let report = doctor_json_from(&nested);
    let finding = finding_by_topic(&report, "agents-cli-config-placement")
        .unwrap_or_else(|| panic!("walk must reach the manifest two levels up: {report}"));

    let action = finding["action"].as_str().unwrap();
    let detail = finding["detail"].as_str().unwrap();
    assert_eq!(
        action,
        "Move keel.toml to app/keel.toml (or add a `COPY keel.toml` line to the Dockerfile).",
        "the agent dir must be named relative to the agents-cli project root"
    );
    assert!(
        detail.ends_with(
            "uv.lock*, and app into the image, so the keel.toml at the project root never \
             ships to the container."
        ),
        "{detail}"
    );
    // The real regression guard: no machine-specific path anywhere in the text.
    let abs_root = std::fs::canonicalize(dir.path()).unwrap();
    let abs_root = abs_root.to_str().unwrap();
    assert!(
        !action.contains(abs_root) && !detail.contains(abs_root),
        "absolute path leaked into --json: {action} / {detail}"
    );
}

/// The other half of the same newly-live walk: a `keel.toml` that is ALREADY
/// inside the agent directory ships fine, so there is no placement problem to
/// report — even though the manifest is two levels up and the walk therefore
/// succeeds. Before the containment fix this emitted a factually wrong warning
/// ("the keel.toml at the project root never ships") about a file that does
/// ship, which is exactly the crying-wolf class WS5 exists to remove.
#[test]
fn doctor_does_not_flag_a_keel_toml_already_inside_the_agent_directory() {
    let dir = agents_cli_tree("app/pkg/keel.toml");
    let nested = dir.path().join("app").join("pkg");
    let report = doctor_json_from(&nested);
    assert!(
        !has_topic(&report, "agents-cli-config-placement"),
        "a keel.toml inside the shipped agent directory is correctly placed: {report}"
    );
}

/// WS1 production pin: `keel run` under an ambient KEEL_CWD that names a
/// directory with no keel.toml must fail before launching anything. Child env
/// only — never `std::env::set_var` (issue #72).
#[test]
fn run_refuses_a_stale_keel_cwd_before_launching() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(dir.path().join("app.py"), "print('RAN')\n").unwrap();
    let stale = tempfile::TempDir::new().unwrap();
    let out = Command::new(keel_bin())
        .current_dir(dir.path())
        .env("KEEL_CWD", stale.path())
        .args(["run", "app.py"])
        .output()
        .expect("spawn keel run");
    assert_eq!(
        out.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("Keel NOT activated"), "{stderr}");
    assert!(
        !String::from_utf8_lossy(&out.stdout).contains("RAN"),
        "target must not run"
    );
}

/// The kill switch must still work under a stale `KEEL_CWD` — that operator
/// (policy never shipped, app misbehaving) is exactly who reaches for
/// `--disable`. README: "`KEEL_DISABLE=1` always wins". Command mode keeps this
/// independent of whether a `keelrun` wheel happens to be importable here.
#[test]
fn run_disable_flag_still_launches_under_a_stale_keel_cwd() {
    let stale = tempfile::TempDir::new().unwrap();
    let out = Command::new(keel_bin())
        .env("KEEL_CWD", stale.path())
        .args(["run", "--disable", "--", "sh", "-c", "echo RAN"])
        .output()
        .expect("spawn keel run --disable");
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("RAN"),
        "the program must run: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// The other arm: no CLI flag at all, just an ambient `KEEL_DISABLE=1`. Both
/// front ends check `is_disabled` before the refusal, so the child would run
/// keel-free; `keel run` must not refuse on its behalf.
#[test]
fn run_ambient_keel_disable_still_launches_under_a_stale_keel_cwd() {
    let stale = tempfile::TempDir::new().unwrap();
    let out = Command::new(keel_bin())
        .env("KEEL_CWD", stale.path())
        .env("KEEL_DISABLE", "1")
        .args(["run", "--", "sh", "-c", "echo RAN"])
        .output()
        .expect("spawn keel run");
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("RAN"),
        "the program must run: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A refused `keel record run` must leave nothing behind — the preflight runs
/// before the recordings directory is created, not after.
#[test]
fn record_run_refused_by_a_stale_keel_cwd_creates_no_recordings_dir() {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(dir.path().join("app.py"), "print('RAN')\n").unwrap();
    let stale = tempfile::TempDir::new().unwrap();
    let out = Command::new(keel_bin())
        .current_dir(dir.path())
        .env("KEEL_CWD", stale.path())
        .args(["record", "run", "app.py"])
        .output()
        .expect("spawn keel record run");
    assert_eq!(
        out.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("Keel NOT activated"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !dir.path().join(".keel").join("recordings").exists(),
        "a refused record must not create .keel/recordings/"
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
