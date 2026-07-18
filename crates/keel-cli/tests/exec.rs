//! `keel exec` integration: a real `/bin/sh` command driven as a `cmd:`
//! durable flow over a real SQLite journal (Task 6.3, WS6, CCR-4). Unix-only:
//! the tests spawn `/bin/sh` and the exec verb's dead-PID probe is unix.

#![cfg(unix)]

use std::path::Path;

/// Seed a `flows` row directly (the technique `resume`-style tests use to
/// simulate a foreign holder) so on_busy/dead-PID paths can be exercised
/// without a second real process. `flow_id` MUST be derived via
/// `keel_cli::exec::identity_flow_id` with the exact same `(flow, command,
/// flow_id_key)` the test's `ExecOptions` uses — a hand-typed `flow_id` that
/// doesn't collide with what `run` looks up makes the test pass vacuously.
fn seed_running_flow(project: &Path, flow_id: &str, holder: &str, lease_expires: i64) {
    let keel = project.join(".keel");
    std::fs::create_dir_all(&keel).unwrap();
    let conn = rusqlite::Connection::open(keel.join("journal.db")).unwrap();
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let schema = std::fs::read_to_string(root.join("contracts/journal.sql")).unwrap();
    let _ = conn.execute_batch(&schema); // idempotent-ish: ignore exists errors
    conn.execute(
        "INSERT OR REPLACE INTO flows \
         (flow_id, entrypoint, args_hash, status, lease_holder, lease_expires, created_at, updated_at) \
         VALUES (?1, 'cmd:busy', 'ah', 'running', ?2, ?3, 1, 1)",
        rusqlite::params![flow_id, holder, lease_expires],
    )
    .unwrap();
}

/// Far enough in the future that no lease-expiry arithmetic in this test
/// suite mistakes it for expired (year ~2286 in ms-epoch).
const FAR_FUTURE_LEASE: i64 = 9_999_999_999_999;

#[test]
fn on_busy_skip_exits_zero_without_running() {
    let dir = tempfile::TempDir::new().unwrap();
    let project = dir.path();
    let marker = project.join("ran.txt");
    let command = vec![
        "/bin/sh".into(),
        "-c".into(),
        format!("echo ran >> {}", marker.display()),
    ];
    let flow_id = keel_cli::exec::identity_flow_id("busy", &command, None);
    // OUR OWN pid as holder: alive by construction, so the dead-PID
    // abandonment path must NOT fire — only on_busy should decide.
    let holder = keel_cli::exec::identity_holder_string(
        &keel_cli::exec::identity_hostname(),
        std::process::id(),
        0,
    );
    seed_running_flow(project, &flow_id, &holder, FAR_FUTURE_LEASE);

    // No keel.toml -> default on_busy = skip.
    let options = keel_cli::exec::ExecOptions {
        flow: "busy".into(),
        flow_id: None,
        journal_files: vec![],
        force: false,
        command,
    };
    let (r, code) = keel_cli::exec::run(project, &options);
    assert_eq!(code, 0);
    let rendered = r.unwrap();
    assert_eq!(rendered.json["skipped"], true);
    assert!(
        !marker.exists(),
        "a skipped busy flow must not run the command"
    );
}

#[test]
fn on_busy_fail_exits_nonzero() {
    let dir = tempfile::TempDir::new().unwrap();
    let project = dir.path();
    let marker = project.join("ran.txt");
    let command = vec![
        "/bin/sh".into(),
        "-c".into(),
        format!("echo ran >> {}", marker.display()),
    ];
    let flow_id = keel_cli::exec::identity_flow_id("busy", &command, None);
    let holder = keel_cli::exec::identity_holder_string(
        &keel_cli::exec::identity_hostname(),
        std::process::id(),
        0,
    );
    seed_running_flow(project, &flow_id, &holder, FAR_FUTURE_LEASE);
    std::fs::write(project.join("keel.toml"), "[flows]\non_busy = \"fail\"\n").unwrap();

    let options = keel_cli::exec::ExecOptions {
        flow: "busy".into(),
        flow_id: None,
        journal_files: vec![],
        force: false,
        command,
    };
    let (r, code) = keel_cli::exec::run(project, &options);
    assert_ne!(code, 0);
    let rendered = r.unwrap();
    assert!(
        rendered.human.contains("KEEL-E030"),
        "on_busy=fail must surface KEEL-E030: {}",
        rendered.human
    );
    assert!(!marker.exists(), "a failed busy-fail flow must not run");
}

#[test]
fn dead_pid_is_abandoned_and_retaken() {
    let dir = tempfile::TempDir::new().unwrap();
    let project = dir.path();
    let marker = project.join("ran.txt");
    let command = vec![
        "/bin/sh".into(),
        "-c".into(),
        format!("echo ran >> {}", marker.display()),
    ];
    let flow_id = keel_cli::exec::identity_flow_id("busy", &command, None);
    // pid 4_000_000 cannot exist on any macOS/Linux test box (above pid_max);
    // FUTURE lease_expires so only the dead-PID probe (not TTL expiry) is
    // exercised.
    let holder =
        keel_cli::exec::identity_holder_string(&keel_cli::exec::identity_hostname(), 4_000_000, 0);
    seed_running_flow(project, &flow_id, &holder, FAR_FUTURE_LEASE);

    let options = keel_cli::exec::ExecOptions {
        flow: "busy".into(),
        flow_id: None,
        journal_files: vec![],
        force: false,
        command,
    };
    let (_r, code) = keel_cli::exec::run(project, &options);
    assert_eq!(code, 0);
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap(),
        "ran\n",
        "the dead holder's lease must be abandoned and the command actually run"
    );
    let conn = rusqlite::Connection::open(project.join(".keel/journal.db")).unwrap();
    let status: String = conn
        .query_row(
            "SELECT status FROM flows WHERE flow_id = ?1",
            rusqlite::params![flow_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(status, "completed");
}

#[test]
fn exec_runs_a_command_as_a_flow_and_replays_when_completed() {
    let dir = tempfile::TempDir::new().unwrap();
    let project = dir.path();
    let marker = project.join("ran.txt");
    let options = keel_cli::exec::ExecOptions {
        flow: "smoke".into(),
        flow_id: Some("t1".into()),
        journal_files: vec![],
        force: false,
        command: vec![
            "/bin/sh".into(),
            "-c".into(),
            format!("echo once >> {}", marker.display()),
        ],
    };
    let (_r, code) = keel_cli::exec::run(project, &options);
    assert_eq!(code, 0);
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "once\n");

    // Same identity again: completed flow -> pure replay, child NOT respawned.
    let (r, code) = keel_cli::exec::run(project, &options);
    assert_eq!(code, 0);
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap(),
        "once\n",
        "no second run"
    );
    assert_eq!(r.unwrap().json["replayed"], true);

    // And the journal shows one flow with one subprocess step.
    let conn = rusqlite::Connection::open(project.join(".keel/journal.db")).unwrap();
    let (entrypoint, status): (String, String) = conn
        .query_row("SELECT entrypoint, status FROM flows", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(entrypoint, "cmd:smoke");
    assert_eq!(status, "completed");
    let kind: String = conn
        .query_row("SELECT kind FROM steps WHERE seq = 1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(kind, "subprocess");
}

#[test]
fn exec_failed_command_exits_failed_and_rerun_reexecutes() {
    let dir = tempfile::TempDir::new().unwrap();
    let project = dir.path();
    let counter = project.join("n.txt");
    let cmd = vec![
        "/bin/sh".into(),
        "-c".into(),
        format!("echo x >> {}; exit 3", counter.display()),
    ];
    let options = keel_cli::exec::ExecOptions {
        flow: "flaky".into(),
        flow_id: Some("t2".into()),
        journal_files: vec![],
        force: false,
        command: cmd,
    };
    let (_r, code) = keel_cli::exec::run(project, &options);
    assert_eq!(code, 3, "child's exit code is keel exec's");

    // Retry re-runs the child (a failed step is NOT replay-substituted).
    let (_r, code) = keel_cli::exec::run(project, &options);
    assert_eq!(code, 3);
    assert_eq!(std::fs::read_to_string(&counter).unwrap(), "x\nx\n");
}

#[test]
fn side_effect_gate_refuses_then_force_overrides() {
    let dir = tempfile::TempDir::new().unwrap();
    let project = dir.path();
    let ledger = project.join("trades.jsonl");
    std::fs::write(&ledger, "").unwrap();
    // A command that WRITES the declared file then fails:
    let cmd = vec![
        "/bin/sh".into(),
        "-c".into(),
        format!("echo t >> {}; exit 5", ledger.display()),
    ];
    let options = keel_cli::exec::ExecOptions {
        flow: "trade".into(),
        flow_id: Some("g1".into()),
        journal_files: vec![ledger.clone()],
        force: false,
        command: cmd,
    };
    let (_r, code) = keel_cli::exec::run(project, &options);
    assert_eq!(code, 5);
    // Retry: the ledger grew during the failed run -> KEEL-E033 refusal,
    // child NOT re-run.
    let (r, code) = keel_cli::exec::run(project, &options);
    assert_ne!(code, 0);
    let rendered = r.unwrap();
    assert!(rendered.human.contains("KEEL-E033"));
    assert_eq!(
        std::fs::read_to_string(&ledger).unwrap(),
        "t\n",
        "not re-run"
    );
    // --force re-dispatches loudly.
    let forced = keel_cli::exec::ExecOptions {
        force: true,
        ..options
    };
    let (_r, code) = keel_cli::exec::run(project, &forced);
    assert_eq!(code, 5);
    assert_eq!(std::fs::read_to_string(&ledger).unwrap(), "t\nt\n");
}

#[test]
fn unchanged_files_do_not_gate() {
    let dir = tempfile::TempDir::new().unwrap();
    let project = dir.path();
    let ledger = project.join("trades.jsonl");
    std::fs::write(&ledger, "").unwrap();
    // A command that fails WITHOUT touching the declared file.
    let cmd = vec!["/bin/sh".into(), "-c".into(), "exit 5".into()];
    let options = keel_cli::exec::ExecOptions {
        flow: "trade2".into(),
        flow_id: Some("g2".into()),
        journal_files: vec![ledger.clone()],
        force: false,
        command: cmd,
    };
    let (_r, code) = keel_cli::exec::run(project, &options);
    assert_eq!(code, 5);
    // Retry proceeds: no declared file changed, so no E033 refusal — the
    // child's own exit code passes through unchanged, not a gate refusal (1).
    let (r, code) = keel_cli::exec::run(project, &options);
    assert_eq!(code, 5, "no gate: the retry actually re-ran the child");
    let rendered = r.unwrap();
    assert!(!rendered.human.contains("KEEL-E033"));
    assert_eq!(
        std::fs::read_to_string(&ledger).unwrap(),
        "",
        "file still empty: the command never touched it"
    );
}
