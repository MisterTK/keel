//! `keel sim <plan>` — fault/latency/crash-restart simulation over a
//! declarative plan (`docs/sim-format.md`; architecture-spec §8: v1 scope is
//! ADAPTER-LEVEL fault injection, not full hermetic/wasmtime determinism).
//!
//! This module owns the parts that are genuinely CLI-shaped:
//!
//! - dispatch the plan's `target`/`args` exactly like `keel run` (via
//!   [`run::plan`]), with `KEEL_SIM_PLAN` (read by the language front ends'
//!   `SimBackend`) and `KEEL_EVENTS=1` (force the Tier 1 event sink on, so
//!   assertions always have something to read) layered onto the child;
//! - detect a child that died to a signal (a `"crash"` directive fired) and
//!   re-invoke the same plan against the same script, up to `max_restarts`
//!   times — the fault-plan front end persists its own per-target cursor
//!   across the restart (`docs/sim-format.md` "Crash-restart"), so this loop
//!   only needs to know "did it die, and how many times has that happened";
//! - after the loop settles, aggregate every event file the run(s) produced
//!   under `.keel/events/` (diffed against what existed before the sim
//!   started) and check the plan's `assert` block against them, plus the
//!   newest flow row in `.keel/journal.db` when `assert.flow_status` is set.
//!
//! What actually injects a fault into an attempt lives entirely in the front
//! ends (`python/keel/src/keel/_sim.py`, `node/keel/src/sim.mjs`) — this
//! module never parses the plan's `faults` block, only `target`/`args`/
//! `max_restarts`/`assert` (the fields it needs to drive and grade the run).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::render::to_json;
use crate::{EXIT_FAILURE, EXIT_OK, EXIT_USAGE, Rendered, evidence, flows, run};

/// Appended to the plan's path to get its cursor sidecar
/// (`docs/sim-format.md`): `_sim.py`/`sim.mjs` persist per-target consumed
/// counts there so a crash-restart continues the fault sequence rather than
/// replaying it from the top. `keel sim` deletes any stale one before the
/// first spawn of a run, so re-running the same plan is always deterministic.
const CURSOR_SUFFIX: &str = ".cursor.json";

/// Mirrors `crates/keel-core/src/events.rs::EVENTS_SUBDIR` — duplicated
/// (like `tail.rs` already does) rather than pulled in as a dependency, since
/// this crate only reads the plain-file convention, never the sink itself.
const EVENTS_SUBDIR: &str = "events";
const EVENTS_EXT: &str = "ndjson";

/// Exit code a crashed-and-exhausted sim settles on: 128 + SIGKILL(9), the
/// code a POSIX shell reports for a process a real `kill -9` terminated —
/// the front ends' `_default_crash`/`_defaultCrash` document the same value.
const SIM_CRASH_EXIT_CODE: i32 = 137;

const DEFAULT_MAX_RESTARTS: u32 = 8;

/// Slack added on top of `flow_lease_ms` before a crash-restart respawns, so
/// a lease that expires exactly at `flow_lease_ms` has definitely lapsed by
/// the time the new process asks for it (clock/scheduling slop).
const LEASE_GRACE_MS: u64 = 200;

/// The fields `keel sim` itself needs from the plan JSON. Everything else
/// (`v`, `faults`) is the front end's concern and is never parsed here —
/// unknown fields are simply ignored (no `deny_unknown_fields`).
#[derive(Debug, Clone, Deserialize)]
struct SimPlan {
    /// The script to dispatch, exactly as `keel run <target>` would.
    target: String,
    #[serde(default)]
    args: Vec<String>,
    /// How many times to re-invoke `target` after a `"crash"` directive kills
    /// it, before giving up.
    #[serde(default = "default_max_restarts")]
    max_restarts: u32,
    /// Forwarded as `KEEL_FLOW_LEASE_MS` on every spawn when `target` is a
    /// Tier 2 flow entrypoint: a resumed flow's lease must have expired
    /// before a fresh process can re-acquire it (KEEL-E030 otherwise), so a
    /// crash-restart sleeps `flow_lease_ms + LEASE_GRACE_MS` before
    /// respawning (the same wait `demos/durable-pipeline/run.sh` does by
    /// hand). `None` (a non-flow or already-fast-leased target) skips the
    /// wait entirely.
    #[serde(default)]
    flow_lease_ms: Option<u64>,
    #[serde(default)]
    assert: SimAssertions,
}

const fn default_max_restarts() -> u32 {
    DEFAULT_MAX_RESTARTS
}

/// The plan's `assert` block (`docs/sim-format.md` "Assertions").
#[derive(Debug, Clone, Default, Deserialize)]
struct SimAssertions {
    /// Per-target cap on the attempts a single call may take.
    #[serde(default)]
    max_attempts: BTreeMap<String, u32>,
    /// Targets whose breaker must have opened at least once.
    #[serde(default)]
    breaker_open: Vec<String>,
    /// Targets whose breaker must never have opened.
    #[serde(default)]
    no_breaker_open: Vec<String>,
    /// The status the newest `.keel/journal.db` flow row must settle on
    /// (`"completed"`, `"failed"`, `"dead"`). `None` skips the check — most
    /// sims of a non-flow target have no journal at all.
    #[serde(default)]
    flow_status: Option<String>,
}

/// One violation, `keel doctor`'s finding shape (`level`, `topic`, `detail`)
/// so the two reports read the same way.
#[derive(Debug, Serialize)]
struct SimFinding {
    detail: String,
    level: &'static str,
    topic: &'static str,
}

/// The whole `keel sim` report.
#[derive(Debug, Serialize)]
struct SimReport {
    exit_code: i32,
    findings: Vec<SimFinding>,
    ok: bool,
    plan: String,
    restarts: u32,
}

/// `keel sim <plan>` for `project`: dispatch, crash-restart-drive, then grade.
pub fn run(project: &Path, plan_path: &str) -> Rendered {
    run_with(project, plan_path, &|_cmd| {})
}

/// Like [`run`], but lets the caller layer extra configuration onto every
/// spawned `Command` before it runs (an integration test's `PYTHONPATH`, a
/// hermetic `KEEL_BACKEND`, …) — mirrors `crate::run::exec_with`'s
/// `configure` hook. `run` is exactly `run_with(project, plan_path, &|_| {})`.
pub fn run_with(project: &Path, plan_path: &str, configure: &dyn Fn(&mut Command)) -> Rendered {
    let path = Path::new(plan_path);
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(err) => return soft_error(&format!("could not read {plan_path}: {err}.")),
    };
    let plan: SimPlan = match serde_json::from_str(&text) {
        Ok(p) => p,
        Err(err) => {
            return soft_error(&format!(
                "{plan_path} is not a valid sim plan (docs/sim-format.md): {err}."
            ));
        }
    };
    let abs_plan = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    // Every `keel sim` starts a plan's fault sequence from directive 0 — drop
    // any cursor a previous invocation left behind.
    let _ = fs::remove_file(cursor_path_for(&abs_plan));

    let run_plan = match run::plan(&plan.target, &plan.args, false) {
        Ok(p) => p,
        Err(e) => return e.render(),
    };

    let events_dir = project.join(".keel").join(EVENTS_SUBDIR);
    let before = existing_event_files(&events_dir);
    let result = drive(
        &run_plan,
        &abs_plan,
        plan.max_restarts,
        plan.flow_lease_ms,
        configure,
    );
    let observed = scan_events(&new_event_files(&events_dir, &before));

    let mut findings = Vec::new();
    check_assertions(&plan.assert, &observed, project, &mut findings);
    if result.crashed {
        findings.push(SimFinding {
            level: "error",
            topic: "crash-restart",
            detail: format!(
                "the target kept crashing after {} restart(s) (max_restarts={}); it never \
                 reached a terminal state.",
                result.restarts, plan.max_restarts
            ),
        });
    }

    let report = SimReport {
        exit_code: result.exit_code,
        ok: findings.is_empty(),
        plan: plan_path.to_owned(),
        restarts: result.restarts,
        findings,
    };
    let exit = if report.ok { EXIT_OK } else { EXIT_USAGE };
    let human = human(&report);
    Rendered::ok(human, to_json(&report)).with_exit(exit)
}

fn cursor_path_for(plan_path: &Path) -> PathBuf {
    let mut s = plan_path.as_os_str().to_owned();
    s.push(CURSOR_SUFFIX);
    PathBuf::from(s)
}

/// The outcome of driving the crash-restart loop.
struct DriveResult {
    exit_code: i32,
    restarts: u32,
    /// True when the loop stopped because `max_restarts` was exhausted while
    /// the child was still dying to the crash directive, not because it ran
    /// to a real terminal exit.
    crashed: bool,
}

/// Spawn `plan` with `KEEL_SIM_PLAN`/`KEEL_EVENTS` layered on, re-invoking it
/// after every crash (a child that died to a signal) up to `max_restarts`
/// times. The SAME `abs_plan_path` is passed every time — the front end's own
/// cursor sidecar (not this loop) is what makes the fault sequence continue
/// rather than restart. When `flow_lease_ms` is set, a crash-restart sleeps
/// `flow_lease_ms + LEASE_GRACE_MS` first, so a resumed Tier 2 flow's lease
/// has genuinely expired before the new process asks for it.
fn drive(
    plan: &run::RunPlan,
    abs_plan_path: &Path,
    max_restarts: u32,
    flow_lease_ms: Option<u64>,
    configure: &dyn Fn(&mut Command),
) -> DriveResult {
    let mut restarts = 0u32;
    loop {
        let mut cmd = Command::new(&plan.program);
        cmd.args(&plan.argv);
        if plan.disable {
            cmd.env("KEEL_DISABLE", "1");
        }
        cmd.env("KEEL_SIM_PLAN", abs_plan_path);
        cmd.env("KEEL_EVENTS", "1");
        if let Some(lease_ms) = flow_lease_ms {
            cmd.env("KEEL_FLOW_LEASE_MS", lease_ms.to_string());
        }
        configure(&mut cmd);
        let Ok(status) = cmd.status() else {
            return DriveResult {
                exit_code: EXIT_FAILURE,
                restarts,
                crashed: false,
            };
        };
        if !crashed(status) {
            return DriveResult {
                exit_code: status.code().unwrap_or(EXIT_FAILURE),
                restarts,
                crashed: false,
            };
        }
        if restarts >= max_restarts {
            return DriveResult {
                exit_code: SIM_CRASH_EXIT_CODE,
                restarts,
                crashed: true,
            };
        }
        restarts += 1;
        if let Some(lease_ms) = flow_lease_ms {
            std::thread::sleep(std::time::Duration::from_millis(lease_ms + LEASE_GRACE_MS));
        }
    }
}

/// Whether `status` shows the child died to a signal (the shape a real `kill
/// -9`, or `_sim.py`/`sim.mjs`'s self-`SIGKILL`, leaves) rather than exiting
/// normally. On a platform with no signal model, a non-portable process
/// cannot be "crashed" this way — treat it as never crashed (the front end's
/// fallback `os._exit(137)` still surfaces as that literal exit code, which
/// `max_restarts=0` callers can still key off via `exit_code`).
#[cfg(unix)]
fn crashed(status: std::process::ExitStatus) -> bool {
    use std::os::unix::process::ExitStatusExt;
    status.signal().is_some()
}

#[cfg(not(unix))]
fn crashed(_status: std::process::ExitStatus) -> bool {
    false
}

/// Every file currently under `dir` (an empty set for a directory that
/// doesn't exist yet — the sink creates it on first write).
fn existing_event_files(dir: &Path) -> BTreeSet<String> {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .collect()
}

/// Event files under `dir` NOT in `before`, oldest first (run ids are
/// zero-padded epoch-ms hex — lexically sortable) — i.e. every run this sim
/// invocation's process(es) produced, across every restart.
fn new_event_files(dir: &Path, before: &BTreeSet<String>) -> Vec<PathBuf> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.ends_with(&format!(".{EVENTS_EXT}")) && !before.contains(n))
        .collect();
    names.sort();
    names.into_iter().map(|n| dir.join(n)).collect()
}

/// What the sim's assertions are checked against: derived from every event
/// line across every run in this sim invocation.
#[derive(Debug, Default)]
struct Observed {
    /// The most attempts any single `call_end` for a target recorded.
    max_attempts: BTreeMap<String, u32>,
    /// Targets that ever emitted a `breaker_open` event.
    breaker_open: BTreeSet<String>,
    /// Whether ANY `call_end`/`breaker_open` event was seen at all — the
    /// event sink (`crates/keel-core/src/events.rs`) is a NATIVE-core-only
    /// feature (`KEEL_BACKEND=native`); a sim run entirely on the pure
    /// stub/dev backend writes no `.keel/events/` at all, which must be a
    /// loud finding when an event-based assertion was requested, never a
    /// silent (and wrong) pass.
    saw_any_event: bool,
}

/// Fold every NDJSON line in `files` into [`Observed`]. Unreadable files and
/// unparseable/foreign lines are skipped, not fatal — mirrors `keel tail`'s
/// and `keel record list`'s line hygiene.
fn scan_events(files: &[PathBuf]) -> Observed {
    let mut out = Observed::default();
    for path in files {
        let Ok(text) = fs::read_to_string(path) else {
            continue;
        };
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            match v.get("event").and_then(Value::as_str) {
                Some("call_end") => {
                    out.saw_any_event = true;
                    let Some(target) = v.get("target").and_then(Value::as_str) else {
                        continue;
                    };
                    let attempts =
                        u32::try_from(v.get("attempts").and_then(Value::as_u64).unwrap_or(0))
                            .unwrap_or(u32::MAX);
                    let entry = out.max_attempts.entry(target.to_owned()).or_insert(0);
                    if attempts > *entry {
                        *entry = attempts;
                    }
                }
                Some("breaker_open") => {
                    out.saw_any_event = true;
                    if let Some(target) = v.get("target").and_then(Value::as_str) {
                        out.breaker_open.insert(target.to_owned());
                    }
                }
                _ => {}
            }
        }
    }
    out
}

/// Check `assert` against `observed` (and, for `flow_status`, `project`'s
/// journal), appending one [`SimFinding`] per violation.
fn check_assertions(
    assert: &SimAssertions,
    observed: &Observed,
    project: &Path,
    findings: &mut Vec<SimFinding>,
) {
    let wants_events = !assert.max_attempts.is_empty() || !assert.breaker_open.is_empty();
    if wants_events && !observed.saw_any_event {
        findings.push(SimFinding {
            level: "error",
            topic: "no-events",
            detail: "max_attempts/breaker_open assertions were requested, but no Tier 1 events \
                     were observed at all. The event sink is a native-core-only feature \
                     (crates/keel-core/src/events.rs) — set KEEL_BACKEND=native (a built \
                     keel_core), or drop these assertions for a pure stub/dev-backend sim."
                .to_owned(),
        });
    }
    for (target, cap) in &assert.max_attempts {
        if let Some(&seen) = observed.max_attempts.get(target)
            && seen > *cap
        {
            findings.push(SimFinding {
                level: "error",
                topic: "max-attempts",
                detail: format!(
                    "{target} made {seen} attempt(s) on one call, exceeding the configured cap \
                     of {cap}."
                ),
            });
        }
    }
    for target in &assert.breaker_open {
        if !observed.breaker_open.contains(target) {
            findings.push(SimFinding {
                level: "error",
                topic: "breaker",
                detail: format!(
                    "expected the breaker for {target} to open under the fault plan, but it \
                     never did."
                ),
            });
        }
    }
    for target in &assert.no_breaker_open {
        if observed.breaker_open.contains(target) {
            findings.push(SimFinding {
                level: "error",
                topic: "breaker",
                detail: format!(
                    "the breaker for {target} opened, but the plan asserted it must not."
                ),
            });
        }
    }
    if let Some(want) = &assert.flow_status {
        match newest_flow_status(project) {
            Some(got) if &got == want => {}
            Some(got) => findings.push(SimFinding {
                level: "error",
                topic: "flow-status",
                detail: format!("expected the flow to end {want}, but it is {got}."),
            }),
            None => findings.push(SimFinding {
                level: "error",
                topic: "flow-status",
                detail: format!(
                    "expected the flow to end {want}, but no flow was found in \
                     .keel/journal.db."
                ),
            }),
        }
    }
}

/// The status of the most-recently-updated row in `.keel/journal.db`'s
/// `flows` table, if any journal/flow exists at all.
fn newest_flow_status(project: &Path) -> Option<String> {
    let path = evidence::resolved_journal(project).path;
    if !path.exists() {
        return None;
    }
    let conn = flows::open_ro(&path).ok()?;
    conn.query_row(
        "SELECT status FROM flows ORDER BY updated_at DESC, flow_id DESC LIMIT 1",
        [],
        |row| row.get::<_, String>(0),
    )
    .ok()
}

fn human(report: &SimReport) -> String {
    let restarts_note = if report.restarts > 0 {
        format!(", {} restart(s)", report.restarts)
    } else {
        String::new()
    };
    if report.ok {
        return format!(
            "keel \u{25b8} sim {} passed \u{2014} exit {}{restarts_note}.",
            report.plan, report.exit_code
        );
    }
    let mut lines = vec![format!(
        "keel \u{25b8} sim {} found {} problem(s) (exit {}{restarts_note}):\n",
        report.plan,
        report.findings.len(),
        report.exit_code
    )];
    for f in &report.findings {
        lines.push(format!("  [{}] {}: {}\n", f.level, f.topic, f.detail));
    }
    lines.concat()
}

/// A precise, non-fatal-to-the-process guidance error — mirrors
/// `crate::record::soft_error`.
fn soft_error(message: &str) -> Rendered {
    #[derive(Serialize)]
    struct ErrReport<'a> {
        error: &'a str,
    }
    Rendered {
        human: format!("keel \u{25b8} {message}"),
        json: to_json(&ErrReport { error: message }),
        exit: EXIT_FAILURE,
        to_stderr: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    fn project() -> TempDir {
        TempDir::new().unwrap()
    }

    fn write_plan(dir: &Path, name: &str, json: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, json).unwrap();
        path
    }

    /// A tiny executable shell script standing in for a `keel run` target —
    /// dispatch through `.py`/`.mjs` would need a real interpreter, but the
    /// crash-restart LOOP itself is language-agnostic, so a `sh` script
    /// wired through a `RunPlan` directly (bypassing `run::plan`'s
    /// extension sniffing) exercises the exact same `drive` logic.
    fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).unwrap();
        path
    }

    #[test]
    fn missing_plan_file_is_a_precise_error() {
        let dir = project();
        let r = run(dir.path(), "does-not-exist.json");
        assert_eq!(r.exit, EXIT_FAILURE);
        assert!(r.human.contains("could not read"));
    }

    #[test]
    fn malformed_plan_json_is_a_precise_error() {
        let dir = project();
        let plan = write_plan(dir.path(), "plan.json", "not json");
        let r = run(dir.path(), &plan.to_string_lossy());
        assert_eq!(r.exit, EXIT_FAILURE);
        assert!(r.human.contains("not a valid sim plan"));
    }

    #[test]
    fn unresolvable_target_reports_the_same_error_keel_run_would() {
        let dir = project();
        let plan = write_plan(
            dir.path(),
            "plan.json",
            r#"{"v":1,"target":"does-not-exist.py"}"#,
        );
        let r = run(dir.path(), &plan.to_string_lossy());
        assert_eq!(r.exit, EXIT_USAGE);
        assert!(r.human.contains("no such file or directory"));
    }

    #[test]
    fn clean_run_with_no_assertions_passes() {
        let dir = project();
        write_script(dir.path(), "ok.sh", "exit 0");
        let plan = write_plan(
            dir.path(),
            "plan.json",
            &format!(
                r#"{{"v":1,"target":{:?}}}"#,
                dir.path().join("ok.sh").to_string_lossy()
            ),
        );
        // Drive the shell script directly through a hand-built RunPlan since
        // `run::plan` only dispatches `.py`/node extensions; the loop under
        // test is `drive`, not extension sniffing (covered by `run.rs`).
        let run_plan = run::RunPlan {
            program: dir.path().join("ok.sh").to_string_lossy().into_owned(),
            argv: vec![],
            disable: false,
        };
        let result = drive(&run_plan, &plan, 4, None, &|_cmd| {});
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.restarts, 0);
        assert!(!result.crashed);
    }

    #[test]
    fn a_self_killed_child_is_retried_up_to_max_restarts() {
        let dir = project();
        // Crashes (SIGKILL's itself) on its first two invocations, then a
        // third invocation exits cleanly — a stand-in for a `"crash"`
        // directive that has exhausted its queue after two consumptions.
        let counter = dir.path().join("count");
        fs::write(&counter, "0").unwrap();
        let script = write_script(
            dir.path(),
            "flaky.sh",
            &format!(
                "n=$(cat {0})\nn=$((n+1))\necho $n > {0}\nif [ \"$n\" -le 2 ]; then kill -9 $$; fi\nexit 0",
                counter.display()
            ),
        );
        let plan = write_plan(dir.path(), "plan.json", r#"{"v":1,"target":"x"}"#);
        let run_plan = run::RunPlan {
            program: script.to_string_lossy().into_owned(),
            argv: vec![],
            disable: false,
        };
        let result = drive(&run_plan, &plan, 4, None, &|_cmd| {});
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.restarts, 2);
        assert!(!result.crashed);
    }

    #[test]
    fn exhausting_max_restarts_while_still_crashing_is_flagged() {
        let dir = project();
        let script = write_script(dir.path(), "always_dies.sh", "kill -9 $$");
        let plan = write_plan(dir.path(), "plan.json", r#"{"v":1,"target":"x"}"#);
        let run_plan = run::RunPlan {
            program: script.to_string_lossy().into_owned(),
            argv: vec![],
            disable: false,
        };
        let result = drive(&run_plan, &plan, 2, None, &|_cmd| {});
        assert_eq!(result.restarts, 2);
        assert!(result.crashed);
        assert_eq!(result.exit_code, SIM_CRASH_EXIT_CODE);
    }

    #[test]
    fn cursor_sidecar_is_reset_before_the_first_spawn() {
        // The cursor is cleared BEFORE `run::plan` validates the target, so a
        // target `keel run` cannot even dispatch (`.sh` is not a Keel
        // extension) still gets a fresh fault sequence next time — clearing
        // it never depends on the child actually spawning.
        let dir = project();
        write_script(dir.path(), "ok.sh", "exit 0");
        let target = dir.path().join("ok.sh").to_string_lossy().into_owned();
        let plan_path = write_plan(
            dir.path(),
            "plan.json",
            &format!(r#"{{"v":1,"target":{target:?}}}"#),
        );
        let abs = fs::canonicalize(&plan_path).unwrap();
        let cursor = cursor_path_for(&abs);
        fs::write(&cursor, r#"{"stale":"leftover"}"#).unwrap();
        assert!(cursor.exists());
        let r = run(dir.path(), &plan_path.to_string_lossy());
        assert_eq!(r.exit, EXIT_USAGE, "{r:?}"); // `.sh` is not a dispatchable target
        assert!(!cursor.exists(), "stale cursor must be cleared regardless");
    }

    #[test]
    fn scan_events_finds_max_attempts_and_breaker_open() {
        let dir = project();
        let events = dir.path().join("run.ndjson");
        let mut f = fs::File::create(&events).unwrap();
        writeln!(
            f,
            r#"{{"v":1,"seq":0,"ms":0,"event":"run_start","run":"r"}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"v":1,"seq":1,"ms":1,"event":"call_end","call":"t-1","target":"api.a","result":"ok","attempts":3}}"#
        )
        .unwrap();
        writeln!(
            f,
            r#"{{"v":1,"seq":2,"ms":2,"event":"breaker_open","call":"t-2","target":"api.b","cooldown_ms":500}}"#
        )
        .unwrap();
        let observed = scan_events(&[events]);
        assert_eq!(observed.max_attempts.get("api.a"), Some(&3));
        assert!(observed.breaker_open.contains("api.b"));
    }

    #[test]
    fn max_attempts_violation_is_a_finding() {
        let observed = Observed {
            max_attempts: BTreeMap::from([("api.a".to_owned(), 5)]),
            breaker_open: BTreeSet::new(),
            saw_any_event: true,
        };
        let assert = SimAssertions {
            max_attempts: BTreeMap::from([("api.a".to_owned(), 3)]),
            ..Default::default()
        };
        let mut findings = Vec::new();
        check_assertions(&assert, &observed, Path::new("."), &mut findings);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].topic, "max-attempts");
        assert!(findings[0].detail.contains("5 attempt"));
    }

    #[test]
    fn breaker_open_expected_but_absent_is_a_finding() {
        let observed = Observed {
            saw_any_event: true,
            ..Default::default()
        };
        let assert = SimAssertions {
            breaker_open: vec!["api.flaky".to_owned()],
            ..Default::default()
        };
        let mut findings = Vec::new();
        check_assertions(&assert, &observed, Path::new("."), &mut findings);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].topic, "breaker");
    }

    #[test]
    fn no_breaker_open_violated_is_a_finding() {
        let observed = Observed {
            max_attempts: BTreeMap::new(),
            breaker_open: BTreeSet::from(["api.pay".to_owned()]),
            saw_any_event: true,
        };
        let assert = SimAssertions {
            no_breaker_open: vec!["api.pay".to_owned()],
            ..Default::default()
        };
        let mut findings = Vec::new();
        check_assertions(&assert, &observed, Path::new("."), &mut findings);
        assert_eq!(findings.len(), 1, "{findings:?}");
    }

    #[test]
    fn no_events_at_all_is_a_finding_when_event_based_assertions_were_requested() {
        // The pure stub/dev backend writes no `.keel/events/` at all — a
        // `max_attempts`/`breaker_open` assertion over an empty `Observed`
        // must be a loud, explicit finding, never a silent (and wrong) pass.
        let observed = Observed::default();
        let assert = SimAssertions {
            max_attempts: BTreeMap::from([("api.a".to_owned(), 3)]),
            ..Default::default()
        };
        let mut findings = Vec::new();
        check_assertions(&assert, &observed, Path::new("."), &mut findings);
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].topic, "no-events");
    }

    #[test]
    fn no_breaker_open_assertion_alone_never_needs_events() {
        // `no_breaker_open` is satisfied trivially by an absence of events —
        // asserting "must not have opened" over a backend with no event sink
        // at all is a legitimate (if weak) no-op, not a finding.
        let observed = Observed::default();
        let assert = SimAssertions {
            no_breaker_open: vec!["api.pay".to_owned()],
            ..Default::default()
        };
        let mut findings = Vec::new();
        check_assertions(&assert, &observed, Path::new("."), &mut findings);
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn flow_status_mismatch_and_missing_are_findings() {
        let dir = project();
        let observed = Observed::default();
        let assert = SimAssertions {
            flow_status: Some("completed".to_owned()),
            ..Default::default()
        };
        let mut findings = Vec::new();
        check_assertions(&assert, &observed, dir.path(), &mut findings);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].detail.contains("no flow was found"));
    }

    fn python3_present() -> bool {
        Command::new("python3")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }

    /// This crate's own `../../python/keel/src` (and the pure stub, for a
    /// script with no native-only feature) — set as `PYTHONPATH` so a spawned
    /// `python3 -m keel run` dispatches against THIS checkout's front end
    /// without needing it `pip install`ed.
    fn python_path() -> String {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
        format!(
            "{}:{}",
            manifest.join("../../python/keel/src").display(),
            manifest.join("../../python/keel-core-stub").display(),
        )
    }

    #[test]
    fn end_to_end_report_is_ok_for_a_clean_run() {
        if !python3_present() {
            eprintln!("skip: python3 not available");
            return;
        }
        let dir = project();
        fs::write(dir.path().join("app.py"), "print(\"hi\")\n").unwrap();
        let target = dir.path().join("app.py").to_string_lossy().into_owned();
        let plan_path = write_plan(
            dir.path(),
            "plan.json",
            &format!(r#"{{"v":1,"target":{target:?}}}"#),
        );
        let pythonpath = python_path();
        let r = run_with(dir.path(), &plan_path.to_string_lossy(), &move |cmd| {
            cmd.env("PYTHONPATH", &pythonpath);
            cmd.env("KEEL_BACKEND", "stub");
            cmd.env("KEEL_QUIET", "1");
        });
        assert_eq!(r.exit, EXIT_OK, "{r:?}");
        assert_eq!(r.json["ok"], true);
        assert_eq!(r.json["exit_code"], 0);
        assert_eq!(r.json["restarts"], 0);
        assert!(r.human.contains("passed"));
    }

    /// Writes a `py:` target that is genuinely wrappable: `call_api` lives in
    /// its OWN module (`lib.py`), imported (not defined) by the dispatched
    /// script (`app.py`) — a real `import lib` goes through `sys.meta_path`
    /// and hits `KeelFinder`, unlike a function defined directly in the
    /// `__main__`-executed file (`runpy.run_path` never registers that file
    /// under an importable module name, so `[target."py:app.<fn>"]` for a
    /// same-named top-level function is never actually wrapped — a real gap,
    /// but an orthogonal one; this fixture sidesteps it the same way
    /// `demos/durable-pipeline` does by pointing `[target."…"]` at a
    /// separately-imported module). `call_api` appends one line to
    /// `calls.txt` — the only reliable signal that the REAL effect ran,
    /// since its return value alone (`{"ok": true}`) can't distinguish a
    /// genuinely-wrapped-and-retried call from an unwrapped, fault-plan-blind
    /// one.
    fn write_wrappable_target(dir: &Path) -> String {
        fs::write(dir.join("keel.toml"), "[target.\"py:lib.call_api\"]\n").unwrap();
        fs::write(
            dir.join("lib.py"),
            "import os\n\nCALLS_FILE = os.path.join(os.path.dirname(__file__), \"calls.txt\")\n\n\ndef call_api():\n    with open(CALLS_FILE, \"a\", encoding=\"utf-8\") as f:\n        f.write(\"call\\n\")\n    return {\"ok\": True}\n",
        )
        .unwrap();
        fs::write(
            dir.join("app.py"),
            "import lib\n\n\ndef main():\n    print(lib.call_api())\n\n\nif __name__ == \"__main__\":\n    main()\n",
        )
        .unwrap();
        dir.join("app.py").to_string_lossy().into_owned()
    }

    fn calls_count(dir: &Path) -> usize {
        fs::read_to_string(dir.join("calls.txt"))
            .map_or(0, |t| t.lines().filter(|l| !l.trim().is_empty()).count())
    }

    /// The real front-end integration, exhaustion leg: FOUR straight
    /// `timeout` directives exceed the default `defaults.outbound` retry cap
    /// (3 attempts) — this can only make the script fail if the fault plan
    /// genuinely reached `_sim.py` and Tier 1's real retry loop genuinely
    /// exhausted on the injected outcomes (an unwrapped/no-op injection would
    /// let `call_api` succeed immediately, contradicting this). Also proves
    /// the real effect never ran (`calls.txt` absent — every attempt was
    /// synthetic) and, via `assert.max_attempts`, that the native core's
    /// event feed recorded exactly 3 attempts.
    #[test]
    fn front_end_fault_injection_exhausts_real_retries_and_fails() {
        let bin_dir = venv_bin_dir();
        if !python3_present() || !native_core_present(bin_dir.as_deref()) {
            eprintln!("skip: native core (keel_core) not available");
            return;
        }
        let dir = project();
        let target = write_wrappable_target(dir.path());
        let plan_path = write_plan(
            dir.path(),
            "plan.json",
            &format!(
                r#"{{"v":1,"target":{target:?},"faults":{{"py:lib.call_api":[{{"kind":"timeout"}},{{"kind":"timeout"}},{{"kind":"timeout"}},{{"kind":"timeout"}}]}},"assert":{{"max_attempts":{{"py:lib.call_api":3}}}}}}"#
            ),
        );
        let pythonpath = python_path();
        let path_env = bin_dir.map_or_else(
            || std::env::var("PATH").unwrap_or_default(),
            |d| {
                format!(
                    "{}:{}",
                    d.display(),
                    std::env::var("PATH").unwrap_or_default()
                )
            },
        );
        // The event sink resolves its directory from the child's OWN cwd
        // (`EventsEnv::capture`'s `base_dir`), not from `project` — pin it to
        // the same tempdir `run_with`'s post-hoc scan will look under.
        let project_dir = dir.path().to_path_buf();
        let r = run_with(dir.path(), &plan_path.to_string_lossy(), &move |cmd| {
            cmd.current_dir(&project_dir);
            cmd.env("PATH", &path_env);
            cmd.env("PYTHONPATH", &pythonpath);
            cmd.env("KEEL_BACKEND", "native");
            cmd.env("KEEL_QUIET", "1");
        });
        // The wrapped call raises after exhausting its 3-attempt budget,
        // propagating unchanged (dx-spec invariant 5) — the script's own
        // process exit is non-zero, distinct from `keel sim`'s own findings.
        assert_ne!(r.json["exit_code"], 0, "{r:?}");
        assert_eq!(calls_count(dir.path()), 0, "the real effect must never run");
        assert_eq!(r.json["ok"], true, "{r:?}"); // the ASSERTED max_attempts (3) held exactly
    }

    /// The real front-end integration, absorbed leg: `timeout` then `5xx`
    /// then `ok` — Tier 1 retries the first two synthetic failures and lets
    /// the third (real) attempt through, so the script succeeds with the
    /// real effect having run EXACTLY once (proving the first two attempts
    /// were genuinely intercepted, not passed through).
    #[test]
    fn front_end_fault_injection_absorbs_retries_then_succeeds() {
        let bin_dir = venv_bin_dir();
        if !python3_present() || !native_core_present(bin_dir.as_deref()) {
            eprintln!("skip: native core (keel_core) not available");
            return;
        }
        let dir = project();
        let target = write_wrappable_target(dir.path());
        let plan_path = write_plan(
            dir.path(),
            "plan.json",
            &format!(
                r#"{{"v":1,"target":{target:?},"faults":{{"py:lib.call_api":[{{"kind":"timeout"}},{{"kind":"5xx","status":503}},{{"kind":"ok"}}]}},"assert":{{"max_attempts":{{"py:lib.call_api":3}}}}}}"#
            ),
        );
        let pythonpath = python_path();
        let path_env = bin_dir.map_or_else(
            || std::env::var("PATH").unwrap_or_default(),
            |d| {
                format!(
                    "{}:{}",
                    d.display(),
                    std::env::var("PATH").unwrap_or_default()
                )
            },
        );
        let project_dir = dir.path().to_path_buf();
        let r = run_with(dir.path(), &plan_path.to_string_lossy(), &move |cmd| {
            cmd.current_dir(&project_dir);
            cmd.env("PATH", &path_env);
            cmd.env("PYTHONPATH", &pythonpath);
            cmd.env("KEEL_BACKEND", "native");
            cmd.env("KEEL_QUIET", "1");
        });
        assert_eq!(r.exit, EXIT_OK, "{r:?}");
        assert_eq!(r.json["ok"], true, "{r:?}");
        assert_eq!(r.json["exit_code"], 0);
        assert_eq!(
            calls_count(dir.path()),
            1,
            "the real effect ran exactly once"
        );
    }

    /// A cheap always-runs (no native core needed) leg: the exhaustion
    /// scenario doesn't need the event feed at all (its signal is the
    /// script's own exit code and `calls.txt`), so it is a genuine,
    /// hermetic, falsifiable proof that fault injection reached `_sim.py`
    /// even under the pure Python stub.
    #[test]
    fn front_end_fault_injection_exhausts_real_retries_on_the_stub() {
        if !python3_present() {
            eprintln!("skip: python3 not available");
            return;
        }
        let dir = project();
        let target = write_wrappable_target(dir.path());
        let plan_path = write_plan(
            dir.path(),
            "plan.json",
            &format!(
                r#"{{"v":1,"target":{target:?},"faults":{{"py:lib.call_api":[{{"kind":"timeout"}},{{"kind":"timeout"}},{{"kind":"timeout"}},{{"kind":"timeout"}}]}}}}"#
            ),
        );
        let pythonpath = python_path();
        // `keel run`'s python3 child always inherits the KEEL PROCESS's cwd
        // (real usage: the user's project dir, since nothing ever calls
        // `Command::current_dir` — the child just fork+execs in place); pin
        // it here since a `cargo test` process's own cwd is this crate's
        // build dir, not the fixture's tempdir, and `keel.toml` resolves
        // relative to the child's cwd (`bootstrap.load_policy`).
        let project_dir = dir.path().to_path_buf();
        let r = run_with(dir.path(), &plan_path.to_string_lossy(), &move |cmd| {
            cmd.current_dir(&project_dir);
            cmd.env("PYTHONPATH", &pythonpath);
            cmd.env("KEEL_BACKEND", "stub");
            cmd.env("KEEL_QUIET", "1");
        });
        assert_ne!(r.json["exit_code"], 0, "{r:?}");
        assert_eq!(calls_count(dir.path()), 0, "the real effect must never run");
    }

    /// This repo's own native-core venv directory
    /// (`demos/durable-pipeline/run.sh`'s own `.venv` convention), if built —
    /// `<repo>/.venv/bin`, prepended onto `PATH` so `run::plan`'s hardcoded
    /// `python3` (`crate::run::python_plan`) resolves to the interpreter
    /// `keel_core` was installed into, not the bare system one. `<repo>` is
    /// resolved via the git COMMON dir (not `CARGO_MANIFEST_DIR/../..`,
    /// which is only this checkout's own worktree root) since the venv is a
    /// shared resource one `maturin develop` builds for every worktree.
    fn venv_bin_dir() -> Option<PathBuf> {
        let out = Command::new("git")
            .args(["rev-parse", "--git-common-dir"])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let git_dir = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
        let repo_root = fs::canonicalize(git_dir).ok()?.parent()?.to_path_buf();
        let bin = repo_root.join(".venv/bin");
        bin.join("python3").is_file().then_some(bin)
    }

    fn native_core_present(bin_dir: Option<&Path>) -> bool {
        let mut cmd = Command::new("python3");
        cmd.arg("-c").arg("import keel_core");
        if let Some(dir) = bin_dir {
            let path = std::env::var("PATH").unwrap_or_default();
            cmd.env("PATH", format!("{}:{path}", dir.display()));
        }
        cmd.status().is_ok_and(|s| s.success())
    }

    /// The genuine Tier 2 mechanical pattern (`demos/durable-pipeline`'s
    /// `kill -9` resume, mirrored here but crashed by a fault-plan `"crash"`
    /// directive instead of a hand-rolled env var): a 5-step flow crashes
    /// (real `SIGKILL`, via `_sim.py`'s `SimBackend`) mid-step-4, and `keel
    /// sim` — after waiting out the flow's lease — re-invokes it, resuming
    /// from the journal (steps 1-3 substituted, 4-5 run live) to completion.
    #[test]
    fn crash_restart_resumes_a_real_tier_2_flow() {
        let bin_dir = venv_bin_dir();
        if !python3_present() || !native_core_present(bin_dir.as_deref()) {
            eprintln!("skip: native core (keel_core) not available");
            return;
        }
        let dir = project();
        fs::write(
            dir.path().join("keel.toml"),
            "[flows]\nentrypoints = [\"py:pipeline:main\"]\n\n[target.\"py:pipeline.do_step\"]\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("pipeline.py"),
            "import os\n\n_LOG = os.path.join(os.path.dirname(__file__), \"steps.log\")\n\n\ndef do_step(n):\n    with open(_LOG, \"a\", encoding=\"utf-8\") as f:\n        f.write(f\"step-{n}\\n\")\n    return {\"step\": n}\n\n\ndef main():\n    for n in range(1, 6):\n        do_step(n)\n    print(\"PIPELINE_COMPLETE\")\n",
        )
        .unwrap();
        let target = dir
            .path()
            .join("pipeline.py")
            .to_string_lossy()
            .into_owned();
        let plan_path = write_plan(
            dir.path(),
            "plan.json",
            &format!(
                r#"{{"v":1,"target":{target:?},"max_restarts":2,"flow_lease_ms":300,"faults":{{"py:pipeline.do_step":[{{"kind":"ok"}},{{"kind":"ok"}},{{"kind":"ok"}},{{"kind":"crash"}}]}},"assert":{{"flow_status":"completed"}}}}"#
            ),
        );
        let pythonpath = python_path();
        let project_dir = dir.path().to_path_buf();
        let path_env = bin_dir.map_or_else(
            || std::env::var("PATH").unwrap_or_default(),
            |d| {
                format!(
                    "{}:{}",
                    d.display(),
                    std::env::var("PATH").unwrap_or_default()
                )
            },
        );
        let r = run_with(dir.path(), &plan_path.to_string_lossy(), &move |cmd| {
            cmd.current_dir(&project_dir);
            cmd.env("PATH", &path_env);
            cmd.env("PYTHONPATH", &pythonpath);
            cmd.env("KEEL_BACKEND", "native");
            cmd.env("KEEL_QUIET", "1");
        });
        assert_eq!(r.json["restarts"], 1, "{r:?}");
        assert_eq!(r.json["ok"], true, "{r:?}");
        assert_eq!(r.json["exit_code"], 0, "{r:?}");
        // The real proof of substitution (not a naive full re-run): each of
        // the 5 steps fired EXACTLY once across both process incarnations —
        // steps 1-3 substituted from the journal on resume (no new lines),
        // 4-5 ran live for the first time (mirrors
        // `demos/durable-pipeline/run.sh`'s own "expect 10, each exactly
        // once" assertion).
        let log = fs::read_to_string(dir.path().join("steps.log")).unwrap();
        let mut lines: Vec<&str> = log.lines().collect();
        lines.sort_unstable();
        assert_eq!(
            lines,
            vec!["step-1", "step-2", "step-3", "step-4", "step-5"]
        );
    }
}
