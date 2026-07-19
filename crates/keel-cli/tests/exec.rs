//! `keel exec` integration: a real `/bin/sh` command driven as a `cmd:`
//! durable flow over a real SQLite journal (Task 6.3, WS6, CCR-4). Unix-only:
//! the tests spawn `/bin/sh` and the exec verb's dead-PID probe is unix.

#![cfg(unix)]

use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

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

// ---- real two-process concurrency (issue #29: "Real two-process `keel exec`
// concurrency test — current on_busy matrix is single-process") -----------
//
// The tests above seed a `flows` row directly and drive `keel_cli::exec::run`
// in-process to *approximate* a foreign holder — they prove `handle_busy`'s
// logic once it observes a held lease, but never prove two independent OS
// processes actually contend for the SAME lease over the SAME SQLite journal
// file at the SAME time, which is the real thing `keel exec` promises
// operators (a cron job, a systemd timer, a second manual invocation)
// at-most-once dispatch under. These tests spawn the real `keel` binary
// (`env!("CARGO_BIN_EXE_keel")`, the convention `tests/mcp.rs` already uses
// for its own real-subprocess test) TWICE against one shared project
// directory: child "A" runs a `cmd:` target whose script writes a marker
// file the instant it starts, then sleeps, so the test can block until A's
// lease is genuinely committed to the on-disk journal before racing child
// "B" against the identical identity (same `--flow` + identical argv ->
// same `args_hash`).

/// The built `keel` binary under test.
fn keel_bin() -> &'static str {
    env!("CARGO_BIN_EXE_keel")
}

/// Spawn `keel exec --flow <flow> -- <command…>` against `project` as a real
/// child process, stdout/stderr piped for later inspection.
fn spawn_exec(project: &Path, flow: &str, command: &[&str]) -> Child {
    Command::new(keel_bin())
        .current_dir(project)
        .arg("exec")
        .arg("--flow")
        .arg(flow)
        .arg("--")
        .args(command)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn keel exec")
}

/// Block until `path` exists or `timeout` elapses — the synchronization
/// barrier proving process A's lease is genuinely committed to the on-disk
/// journal (A's script touches the marker only AFTER `keel exec` has already
/// entered the flow and recorded the `running` step; see
/// `exec.rs::live_run`, which records the step BEFORE spawning the child)
/// before process B is spawned to race it.
fn wait_for_marker(path: &Path, timeout: Duration) {
    let start = Instant::now();
    while !path.exists() {
        assert!(
            start.elapsed() < timeout,
            "process A never reached its start marker ({}) within {:?}",
            path.display(),
            timeout
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Poll `child` to completion without ever blocking indefinitely (an
/// `on_busy = wait` regression that never released the lease would otherwise
/// hang `cargo test` rather than fail it). Output is read only after exit,
/// which is safe here because these fixture commands emit at most a few
/// bytes — nowhere near a pipe's buffer limit.
fn wait_bounded(mut child: Child, timeout: Duration) -> Output {
    use std::io::Read;
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            if let Some(mut s) = child.stdout.take() {
                let _ = s.read_to_end(&mut stdout);
            }
            if let Some(mut s) = child.stderr.take() {
                let _ = s.read_to_end(&mut stderr);
            }
            return Output {
                status,
                stdout,
                stderr,
            };
        }
        assert!(
            start.elapsed() < timeout,
            "child process did not exit within {timeout:?} \u{2014} on_busy=wait likely never \
             released the lease"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A `/bin/sh -c` script that proves it started (writes `started`) before
/// sleeping ~1s and then recording its one real side effect (appending to
/// `ran`) — the slow, observably-busy command the racing process B contends
/// against.
fn slow_marked_script(started: &Path, ran: &Path) -> String {
    format!(
        ": > {}; sleep 1; echo a >> {}",
        started.display(),
        ran.display()
    )
}

#[test]
fn two_processes_race_on_busy_skip_second_process_never_runs() {
    let dir = tempfile::TempDir::new().unwrap();
    let project = dir.path();
    let started = project.join("a-started");
    let ran = project.join("ran.txt");
    let script = slow_marked_script(&started, &ran);
    let argv = ["/bin/sh", "-c", script.as_str()];

    // No keel.toml -> default on_busy = skip.
    let proc_a = spawn_exec(project, "race", &argv);
    wait_for_marker(&started, Duration::from_secs(10));

    // Process B: a genuinely separate OS process racing A's still-live lease
    // over the SAME shared journal file, for the SAME identity (same --flow
    // + identical argv).
    let out_b = spawn_exec(project, "race", &argv)
        .wait_with_output()
        .expect("wait for process B");
    assert!(
        out_b.status.success(),
        "on_busy=skip must exit 0: {}",
        String::from_utf8_lossy(&out_b.stderr)
    );
    let stdout_b = String::from_utf8_lossy(&out_b.stdout);
    assert!(
        stdout_b.contains("is busy") && stdout_b.contains("flows.on_busy = skip"),
        "process B must report the busy-skip decision: {stdout_b}"
    );

    let out_a = proc_a.wait_with_output().expect("wait for process A");
    assert!(
        out_a.status.success(),
        "process A must complete normally: {}",
        String::from_utf8_lossy(&out_a.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(&ran).unwrap(),
        "a\n",
        "process B (skip) must never have run the command \u{2014} exactly one real execution, \
         in A, under genuine cross-process contention"
    );
}

#[test]
fn two_processes_race_on_busy_fail_second_process_exits_nonzero() {
    let dir = tempfile::TempDir::new().unwrap();
    let project = dir.path();
    std::fs::write(project.join("keel.toml"), "[flows]\non_busy = \"fail\"\n").unwrap();
    let started = project.join("a-started");
    let ran = project.join("ran.txt");
    let script = slow_marked_script(&started, &ran);
    let argv = ["/bin/sh", "-c", script.as_str()];

    let proc_a = spawn_exec(project, "race", &argv);
    wait_for_marker(&started, Duration::from_secs(10));

    let out_b = spawn_exec(project, "race", &argv)
        .wait_with_output()
        .expect("wait for process B");
    assert!(!out_b.status.success(), "on_busy=fail must exit nonzero");
    let stderr_b = String::from_utf8_lossy(&out_b.stderr);
    assert!(
        stderr_b.contains("KEEL-E030") && stderr_b.contains("flows.on_busy = fail"),
        "process B's stderr must surface KEEL-E030 under on_busy=fail: {stderr_b}"
    );

    let out_a = proc_a.wait_with_output().expect("wait for process A");
    assert!(
        out_a.status.success(),
        "process A must complete normally: {}",
        String::from_utf8_lossy(&out_a.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(&ran).unwrap(),
        "a\n",
        "process B (fail) must never have run the command under genuine cross-process contention"
    );
}

#[test]
fn two_processes_race_on_busy_wait_second_process_replays_after_first_completes() {
    let dir = tempfile::TempDir::new().unwrap();
    let project = dir.path();
    std::fs::write(project.join("keel.toml"), "[flows]\non_busy = \"wait\"\n").unwrap();
    let started = project.join("a-started");
    let ran = project.join("ran.txt");
    let script = slow_marked_script(&started, &ran);
    let argv = ["/bin/sh", "-c", script.as_str()];

    let proc_a = spawn_exec(project, "race", &argv);
    wait_for_marker(&started, Duration::from_secs(10));

    // Process B waits out A's live lease (bounded so a real on_busy=wait
    // regression fails this test instead of hanging `cargo test`).
    let child_b = spawn_exec(project, "race", &argv);
    let out_b = wait_bounded(child_b, Duration::from_secs(20));
    assert!(
        out_b.status.success(),
        "process B must eventually succeed once A's lease is released: {}",
        String::from_utf8_lossy(&out_b.stderr)
    );
    let stdout_b = String::from_utf8_lossy(&out_b.stdout);
    assert!(
        stdout_b.contains("already completed") && stdout_b.contains("replaying recorded outcome"),
        "process B must replay A's SAME completed identity rather than re-run the command: \
         {stdout_b}"
    );

    let out_a = proc_a.wait_with_output().expect("wait for process A");
    assert!(
        out_a.status.success(),
        "process A must complete normally: {}",
        String::from_utf8_lossy(&out_a.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(&ran).unwrap(),
        "a\n",
        "process B (wait) must replay, not re-execute \u{2014} exactly one real execution, in A, \
         under genuine cross-process contention"
    );
}
