//! `keel flows force <flow-id>` integration: the durable, out-of-process
//! KEEL-E033 escape hatch (CCR-5 decision 2 / CCR-6), proven end-to-end
//! through the real `keel` binary against a real SQLite journal.
//!
//! The core proof (`force_then_one_more_dispatch_proceeds_then_gates_again`)
//! mirrors `exec.rs`'s `side_effect_gate_refuses_then_force_overrides`, but
//! with the *CLI verb* — not the in-memory `--force` flag — as the override in
//! the middle: seed a `cmd:` flow whose declared file changed on a failed run
//! (so a bare retry is KEEL-E033-refused), run `keel flows force <id>` as a
//! real subprocess, confirm the very next re-dispatch now proceeds, and confirm
//! a THIRD attempt (no fresh force) is refused again. That is the one-shot
//! guarantee end-to-end through the actual binary.
//!
//! Unix-only: the exec verb it drives spawns `/bin/sh` and its dead-PID probe
//! is unix (same gate as `tests/exec.rs`).

#![cfg(unix)]

use std::path::Path;
use std::process::{Command, Output};

/// The built `keel` binary under test (the convention `tests/exec.rs` uses).
fn keel_bin() -> &'static str {
    env!("CARGO_BIN_EXE_keel")
}

/// Run `keel flows force <flow>` against `project` as a real child process and
/// collect its output — the exact path an operator's `keel flows force`
/// invocation takes.
fn run_flows_force(project: &Path, flow: &str) -> Output {
    Command::new(keel_bin())
        .current_dir(project)
        .arg("flows")
        .arg("force")
        .arg(flow)
        .output()
        .expect("spawn keel flows force")
}

/// A `cmd:` flow whose declared side-effect file is APPENDED to and then the
/// command fails — so the pre-run snapshot vs. post-run state differ and any
/// retry is KEEL-E033-gated. Driven in-process via `keel_cli::exec::run`
/// (identical journal-on-disk to a real `keel exec`; the file lives under
/// `project`, so the real `keel flows force` subprocess sees the same journal).
fn gated_options(ledger: &Path) -> keel_cli::exec::ExecOptions {
    keel_cli::exec::ExecOptions {
        flow: "trade".into(),
        flow_id: Some("g1".into()),
        journal_files: vec![ledger.to_path_buf()],
        force: false,
        command: vec![
            "/bin/sh".into(),
            "-c".into(),
            format!("echo t >> {}; exit 5", ledger.display()),
        ],
    }
}

#[test]
fn force_then_one_more_dispatch_proceeds_then_gates_again() {
    let dir = tempfile::TempDir::new().unwrap();
    let project = dir.path();
    let ledger = project.join("trades.jsonl");
    std::fs::write(&ledger, "").unwrap();
    let options = gated_options(&ledger);

    // 1. First dispatch: writes the declared file, then exits 5 (a failed run).
    let (_r, code) = keel_cli::exec::run(project, &options);
    assert_eq!(code, 5);
    assert_eq!(std::fs::read_to_string(&ledger).unwrap(), "t\n");

    // 2. Bare retry: the ledger grew during the failed run → KEEL-E033 refusal,
    //    the child is NOT re-run.
    let (r, code) = keel_cli::exec::run(project, &options);
    assert_ne!(code, 0);
    assert!(r.unwrap().human.contains("KEEL-E033"));
    assert_eq!(
        std::fs::read_to_string(&ledger).unwrap(),
        "t\n",
        "not re-run"
    );

    // 3. Arm the durable one-shot through the REAL CLI binary.
    let flow_id = keel_cli::exec::identity_flow_id("trade", &options.command, Some("g1"));
    let forced = run_flows_force(project, &flow_id);
    assert!(
        forced.status.success(),
        "keel flows force must exit 0: {}",
        String::from_utf8_lossy(&forced.stderr)
    );
    let stdout = String::from_utf8_lossy(&forced.stdout);
    assert!(
        stdout.contains(&flow_id) && stdout.contains("armed"),
        "keel flows force must report the armed flow: {stdout}"
    );

    // 4. The next re-dispatch now PROCEEDS once (the child runs, the ledger
    //    grows again) — the override let exactly this attempt through the gate.
    let (_r, code) = keel_cli::exec::run(project, &options);
    assert_eq!(code, 5, "the forced re-dispatch actually re-ran the child");
    assert_eq!(
        std::fs::read_to_string(&ledger).unwrap(),
        "t\nt\n",
        "the forced attempt re-ran the command exactly once"
    );

    // 5. One-shot: a THIRD attempt with no fresh `keel flows force` is refused
    //    again — the override was spent by step 4, not left armed.
    let (r, code) = keel_cli::exec::run(project, &options);
    assert_ne!(code, 0);
    assert!(
        r.unwrap().human.contains("KEEL-E033"),
        "the one-shot must be spent: a second forced bypass requires re-arming"
    );
    assert_eq!(
        std::fs::read_to_string(&ledger).unwrap(),
        "t\nt\n",
        "the re-gated attempt did not re-run the command"
    );
}

#[test]
fn force_then_json_reports_the_armed_flow() {
    let dir = tempfile::TempDir::new().unwrap();
    let project = dir.path();
    let ledger = project.join("trades.jsonl");
    std::fs::write(&ledger, "").unwrap();
    let options = gated_options(&ledger);

    // Seed a real failed cmd: flow so there is something to force.
    let (_r, code) = keel_cli::exec::run(project, &options);
    assert_eq!(code, 5);

    let flow_id = keel_cli::exec::identity_flow_id("trade", &options.command, Some("g1"));
    let out = Command::new(keel_bin())
        .current_dir(project)
        .arg("--json")
        .arg("flows")
        .arg("force")
        .arg(&flow_id)
        .output()
        .expect("spawn keel --json flows force");
    assert!(out.status.success());
    let json: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("--json flows force emits valid JSON");
    assert_eq!(json["armed"], true);
    assert_eq!(json["flow_id"], flow_id);
}

#[test]
fn forcing_a_nonexistent_flow_is_a_clean_error_not_a_panic() {
    let dir = tempfile::TempDir::new().unwrap();
    let project = dir.path();
    let ledger = project.join("trades.jsonl");
    std::fs::write(&ledger, "").unwrap();
    // A real journal exists (with one flow), but this id matches nothing.
    let (_r, code) = keel_cli::exec::run(project, &gated_options(&ledger));
    assert_eq!(code, 5);

    let out = run_flows_force(project, "definitely-not-a-real-flow-id");
    assert!(!out.status.success(), "an unknown flow must exit nonzero");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no flow matches"),
        "a clean what/why/next, not a raw SQL error or panic: {stderr}"
    );
    assert!(!stderr.contains("panicked"), "must never panic: {stderr}");
}

#[test]
fn forcing_a_completed_flow_says_nothing_to_force() {
    let dir = tempfile::TempDir::new().unwrap();
    let project = dir.path();
    // A command that SUCCEEDS with no declared files → the flow completes.
    let options = keel_cli::exec::ExecOptions {
        flow: "done".into(),
        flow_id: Some("c1".into()),
        journal_files: vec![],
        force: false,
        command: vec!["/bin/sh".into(), "-c".into(), "true".into()],
    };
    let (_r, code) = keel_cli::exec::run(project, &options);
    assert_eq!(code, 0);

    let flow_id = keel_cli::exec::identity_flow_id("done", &options.command, Some("c1"));
    let out = run_flows_force(project, &flow_id);
    assert!(
        !out.status.success(),
        "forcing a completed flow is refused (nothing to force)"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("already completed") && stderr.contains("nothing to force"),
        "a completed flow's force refusal must explain why: {stderr}"
    );
}

#[test]
fn force_without_a_journal_is_a_clean_soft_error() {
    let dir = tempfile::TempDir::new().unwrap();
    let out = run_flows_force(dir.path(), "anything");
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("no journal yet"),
        "no journal → a clean soft error, not a panic: {stderr}"
    );
}
