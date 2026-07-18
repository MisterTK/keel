//! `keel exec` integration: a real `/bin/sh` command driven as a `cmd:`
//! durable flow over a real SQLite journal (Task 6.3, WS6, CCR-4). Unix-only:
//! the tests spawn `/bin/sh` and the exec verb's dead-PID probe is unix.

#![cfg(unix)]

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
