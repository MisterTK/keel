//! `keel exec --flow <name> -- <program> [args…]` — wrap one external command
//! as a journaled Tier-2 durable flow (CCR-4).
//!
//! What `keel exec` gives an arbitrary process that has no Keel front end (a
//! shell script, a `uvx` server, a cron job): **at-most-once dispatch per
//! identity** and **crash-safe retry gating**. It is emphatically NOT
//! exactly-once execution *inside* the child — Keel cannot un-send whatever
//! side effects a subprocess performs. The honest guarantee is at the
//! dispatch boundary: a completed flow never re-runs its command (pure
//! replay), a live holder is fenced or waited on per policy, and a re-dispatch
//! after a failure is gated by a declared-side-effect snapshot compare
//! (KEEL-E033) and the flow-level attempt cap (KEEL-E032).
//!
//! # Identity and fencing
//!
//! Flow identity is `(cmd:<name>, args_hash, --flow-id?)`; `args_hash =
//! sha256(argv.join("\0"))[..16]` makes two different command lines distinct
//! flows, and `code_hash = sha256(resolved_program_path + "\0" +
//! argv.join("\0"))[..16]` fences replay when the program itself changes on
//! disk. The lease holder is recorded as `"{hostname}:{pid}:{started_ms}"`;
//! `started_ms` is kept for PID-reuse forensics but is not consulted in v1 —
//! the lease TTL remains the reuse backstop.
//!
//! # Why the single step is driven through the `Journal` directly, not
//! [`execute_step`](keel_core::FlowHandle::execute_step)
//!
//! A terminal `error` record would be replay-SUBSTITUTED on the next entry
//! (rule 3 of the tier-2 semantics), so a failed command could never be re-run
//! — but re-running after a failure IS `keel exec`'s retry model (gated by the
//! snapshot compare + attempt cap). The single step is therefore driven
//! through the [`Journal`] directly; [`FlowManager`] still owns
//! identity/lease/attempts/completion, and a crashed run (a `running` step)
//! resumes exactly like any tier-2 crash. Substitution is honored where it is
//! correct: the completed-flow pure-replay path. This is the one-step v1 shape;
//! multi-step `cmd` flows would revisit it.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use keel_core::{Engine, FlowDescriptor, FlowHandle, FlowManager};
use keel_core_api::policy::OnBusy;
use keel_core_api::{ErrorClass, ErrorCode};
use keel_journal::{
    Clock, FlowId, FlowStatus, Journal, ProcessId, SqliteJournal, StepKey, StepKind, StepOutcome,
    StepStatus, SystemClock,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::render::to_json;
use crate::{EXIT_FAILURE, EXIT_OK, EXIT_USAGE, Rendered, evidence, flows};

/// The reserved marker seq holding the pre-run declared-side-effect snapshot.
/// High above any real step count (steps are numbered from 1) and below
/// [`keel_core`]'s branch lane (`BRANCH_SEQ_BASE = 1_000_000`) so the two
/// reserved lanes never collide.
const SNAP_BEFORE_SEQ: u64 = 500_000;
/// The reserved marker seq holding the post-run snapshot (the record
/// `keel trace` shows as the flow's after-state).
const SNAP_AFTER_SEQ: u64 = 500_001;

/// The single command step's seq. A `cmd` flow has exactly one step in v1.
const STEP_SEQ: u64 = 1;

/// How many trailing bytes of the child's stdout/stderr are captured into the
/// step's trace tail (the live view is still forwarded byte-for-byte).
const TAIL_CAP: usize = 4096;

/// The schema tag stamped into every step payload — identical to
/// `keel-core/src/flow.rs::STEP_PAYLOAD_SCHEMA` and `replay.rs`, so `keel
/// replay --step` decodes an exec step's payload the same way it decodes any
/// other step's.
const STEP_PAYLOAD_SCHEMA: &str = "keel.step/v1";

/// `keel exec` options (the parsed clap surface).
#[derive(Debug, Clone)]
pub struct ExecOptions {
    /// Flow name; becomes the `cmd:<name>` entrypoint. `[a-z0-9][a-z0-9-]*`.
    pub flow: String,
    /// Explicit flow identity key (default: derived from name + argv).
    pub flow_id: Option<String>,
    /// Declared side-effect files: line count + content hash recorded
    /// before/after; a change across a failed run gates re-dispatch (E033).
    pub journal_files: Vec<PathBuf>,
    /// Override the KEEL-E033 side-effect gate and re-dispatch anyway.
    pub force: bool,
    /// The command to run, after `--` (program then args).
    pub command: Vec<String>,
}

// ---- pure, unit-testable pieces ------------------------------------------

/// Whether `name` matches the CCR flow-name grammar `[a-z0-9][a-z0-9-]*`.
fn valid_flow_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// The first 16 hex chars of `sha256(bytes)` — the identity digest width.
fn sha16(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())[..16].to_owned()
}

/// `args_hash`: the identity digest over the full argv (NUL-joined so no argv
/// value can forge a boundary).
fn args_hash(command: &[String]) -> String {
    sha16(command.join("\u{0}").as_bytes())
}

/// `code_hash`: fences replay across a changed program binary — the resolved
/// program path plus the argv.
fn code_hash(command: &[String]) -> String {
    let program = resolve_program(&command[0]);
    sha16(format!("{program}\u{0}{}", command.join("\u{0}")).as_bytes())
}

/// argv[0] through a PATH lookup (`which`-style); unresolvable -> verbatim (the
/// hash still fences argv changes).
fn resolve_program(argv0: &str) -> String {
    if argv0.contains('/') {
        return argv0.to_owned();
    }
    let Some(path) = std::env::var_os("PATH") else {
        return argv0.to_owned();
    };
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(argv0);
        if candidate.is_file() {
            return candidate.display().to_string();
        }
    }
    argv0.to_owned()
}

/// A parsed `keel exec` lease holder.
#[derive(Debug)]
struct Holder<'a> {
    host: &'a str,
    pid: u32,
}

/// Format a lease holder: `"{hostname}:{pid}:{started_ms}"`.
fn holder_string(host: &str, pid: u32, started_ms: i64) -> String {
    format!("{host}:{pid}:{started_ms}")
}

/// Parse a `keel exec` holder (`"host:pid:started_ms"`). Legacy/foreign holder
/// formats yield `None` — then only the lease TTL arbitrates.
fn parse_holder(s: &str) -> Option<Holder<'_>> {
    let mut parts = s.rsplitn(3, ':');
    let _started: i64 = parts.next()?.parse().ok()?;
    let pid: u32 = parts.next()?.parse().ok()?;
    let host = parts.next()?;
    Some(Holder { host, pid })
}

/// A declared side-effect file's recorded shape (line count + content hash).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FileSnap {
    path: String,
    exists: bool,
    lines: u64,
    sha256: String,
}

/// Snapshot each declared file's current `(exists, line_count, sha256)`.
// `naive_bytecount`: adding the `bytecount` crate for a snapshot taken once per
// exec (not a hot path) is not worth a new dependency.
#[allow(clippy::naive_bytecount)]
fn snapshot_files(files: &[PathBuf]) -> Vec<FileSnap> {
    files
        .iter()
        .map(|p| match std::fs::read(p) {
            Ok(bytes) => FileSnap {
                path: p.display().to_string(),
                exists: true,
                lines: bytes.iter().filter(|&&b| b == b'\n').count() as u64,
                sha256: sha16(&bytes),
            },
            Err(_) => FileSnap {
                path: p.display().to_string(),
                exists: false,
                lines: 0,
                sha256: String::new(),
            },
        })
        .collect()
}

/// Paths whose recorded snapshot differs from the current one. A file with no
/// recorded entry is NOT flagged (newly declared — nothing to compare).
fn changed_files(recorded: &[FileSnap], current: &[FileSnap]) -> Vec<String> {
    current
        .iter()
        .filter(|now| {
            recorded
                .iter()
                .find(|r| r.path == now.path)
                .is_some_and(|r| r != *now)
        })
        .map(|s| s.path.clone())
        .collect()
}

// ---- payload codec (mirrors keel-core's schema-tagged envelope) -----------

/// The schema-tagged step-payload envelope, written by reference (no clone).
#[derive(Serialize)]
struct StepPayloadRef<'a> {
    schema: &'a str,
    payload: &'a Value,
}

/// The owned form read back before its tag is verified.
#[derive(Deserialize)]
struct StepPayloadOwned {
    schema: String,
    payload: Value,
}

/// MessagePack-encode a step payload with its schema tag (journal.sql:
/// `steps.payload` is "MessagePack, schema-tagged") — the exact convention the
/// core writes and `replay.rs` reads.
fn encode_payload(value: &Value) -> Option<Vec<u8>> {
    rmp_serde::to_vec_named(&StepPayloadRef {
        schema: STEP_PAYLOAD_SCHEMA,
        payload: value,
    })
    .ok()
}

/// Decode a step payload: the schema-tagged envelope, else a bare value.
fn decode_payload(bytes: &[u8]) -> Option<Value> {
    if let Ok(envelope) = rmp_serde::from_slice::<StepPayloadOwned>(bytes)
        && envelope.schema == STEP_PAYLOAD_SCHEMA
    {
        return Some(envelope.payload);
    }
    rmp_serde::from_slice(bytes).ok()
}

// ---- host/process probes (the only unsafe in this crate) ------------------

/// This host's name, for fencing the dead-PID probe to the machine that
/// recorded the holder. Uses `gethostname` on unix; elsewhere (and on any
/// failure) an environment fallback. v1 simplification: the value only needs to
/// be *stable per host* across two exec invocations — a shared journal spanning
/// hosts that both fall back to `"localhost"` would be a false host match, but
/// the v1 `file:` journal is machine-local.
#[cfg(unix)]
fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: `gethostname` writes at most `buf.len()` bytes into `buf` and
    // NUL-terminates within it; we only read the returned prefix up to the NUL.
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc == 0 {
        let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        if let Ok(s) = std::str::from_utf8(&buf[..end])
            && !s.is_empty()
        {
            return s.to_owned();
        }
    }
    env_hostname()
}

#[cfg(not(unix))]
fn hostname() -> String {
    env_hostname()
}

/// Best-effort hostname from the environment, defaulting to `"localhost"`.
fn env_hostname() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "localhost".to_owned())
}

/// Whether `pid` is definitively gone on this host: `kill(pid, 0)` failing with
/// `ESRCH`. A live process (or `EPERM`) reads as alive. Non-unix cannot probe
/// portably, so it treats the holder as alive (the lease TTL is the backstop).
#[cfg(unix)]
fn pid_is_dead(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: `kill` with signal 0 performs the existence/permission check
    // without delivering a signal; it has no memory effects.
    let rc = unsafe { libc::kill(pid, 0) };
    rc != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

#[cfg(not(unix))]
fn pid_is_dead(_pid: u32) -> bool {
    false
}

// ---- child process (tee-and-tail) ----------------------------------------

/// The outcome of running the child: its exit code, the trailing tails, and
/// whether the spawn itself failed (127).
struct SpawnResult {
    exit_code: i32,
    stdout_tail: String,
    stderr_tail: String,
    spawn_failed: bool,
}

/// Forward `reader` to the parent's `stdout`/`stderr` byte-for-byte while
/// keeping the last [`TAIL_CAP`] bytes as a decoded (lossy) tail.
fn tee<R: Read>(mut reader: R, to_stderr: bool) -> String {
    let mut ring: Vec<u8> = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let chunk = &buf[..n];
                if to_stderr {
                    let _ = std::io::stderr().write_all(chunk);
                } else {
                    let _ = std::io::stdout().write_all(chunk);
                }
                ring.extend_from_slice(chunk);
                if ring.len() > TAIL_CAP {
                    let drop = ring.len() - TAIL_CAP;
                    ring.drain(..drop);
                }
            }
        }
    }
    String::from_utf8_lossy(&ring).into_owned()
}

/// The child's exit code, mapping a signal death to `128 + signal` on unix.
fn exit_code_of(status: std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status
            .code()
            .unwrap_or_else(|| status.signal().map_or(1, |s| 128 + s))
    }
    #[cfg(not(unix))]
    {
        status.code().unwrap_or(1)
    }
}

/// Spawn `command`, teeing stdout/stderr to the parent while capturing tails.
fn run_child(command: &[String]) -> SpawnResult {
    use std::process::{Command, Stdio};
    let mut cmd = Command::new(&command[0]);
    cmd.args(&command[1..])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("keel \u{25b8} exec: could not spawn `{}`: {e}", command[0]);
            return SpawnResult {
                exit_code: 127,
                stdout_tail: String::new(),
                stderr_tail: String::new(),
                spawn_failed: true,
            };
        }
    };
    let out = child.stdout.take().expect("stdout piped");
    let err = child.stderr.take().expect("stderr piped");
    let out_h = std::thread::spawn(move || tee(out, false));
    let err_h = std::thread::spawn(move || tee(err, true));
    let status = child.wait();
    let stdout_tail = out_h.join().unwrap_or_default();
    let stderr_tail = err_h.join().unwrap_or_default();
    let exit_code = status.map_or(127, exit_code_of);
    SpawnResult {
        exit_code,
        stdout_tail,
        stderr_tail,
        spawn_failed: false,
    }
}

// ---- the report ----------------------------------------------------------

/// The `keel exec` `--json` twin.
#[derive(Debug, Serialize)]
struct ExecReport {
    exit_code: i32,
    flow_id: String,
    entrypoint: String,
    replayed: bool,
    skipped: bool,
    forced: bool,
    journal_files: Vec<FileSnap>,
}

impl ExecReport {
    fn render(self, human: String, to_stderr: bool) -> Rendered {
        let exit = self.exit_code;
        Rendered {
            human,
            json: to_json(&self),
            exit,
            to_stderr,
        }
    }
}

/// A usage error (exit 2, on stderr) — mirrors `resume.rs`'s `usage_pair`.
fn usage_pair(message: &str) -> (Option<Rendered>, i32) {
    #[derive(Serialize)]
    struct UsageReport<'a> {
        error: &'static str,
        what: &'a str,
    }
    let r = Rendered {
        human: format!("keel \u{25b8} {message}"),
        json: to_json(&UsageReport {
            error: "bad-usage",
            what: message,
        }),
        exit: EXIT_USAGE,
        to_stderr: true,
    };
    (Some(r), EXIT_USAGE)
}

/// A soft error (exit 1, on stderr) — mirrors `keel status`/`resume`.
fn soft_pair(message: &str) -> (Option<Rendered>, i32) {
    let r = flows::soft_error(message);
    let code = r.exit;
    (Some(r), code)
}

// ---- orchestration -------------------------------------------------------

/// `keel exec` for `project`: wrap one command as a `cmd:` durable flow.
///
/// Returns the rendered report (if any) and the process exit code: the child's
/// own exit code on a live/replay run, 127 on a spawn failure, 0 on a
/// busy-skip, 1 on a refusal (E032/E033/E030-fail), 2 on a usage error.
pub fn run(project: &Path, options: &ExecOptions) -> (Option<Rendered>, i32) {
    // 1. Validate the name and the command.
    if !valid_flow_name(&options.flow) {
        return usage_pair(&format!(
            "--flow must match [a-z0-9][a-z0-9-]* (the CCR flow-name grammar); got {:?}.",
            options.flow
        ));
    }
    if options.command.is_empty() {
        return usage_pair(
            "the command after `--` must be non-empty: `keel exec --flow <name> -- <program> \
             [args…]`.",
        );
    }

    // 2. Open the journal READ-WRITE, the way keel-core's journal_backend does
    //    for a `file:` location. Every exec-time read AND write goes through
    //    this ONE instance (issue #14: no second in-process reader).
    let journal_path = evidence::resolved_journal(project).path;
    if let Some(parent) = journal_path.parent()
        && !parent.as_os_str().is_empty()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        return soft_pair(&format!("could not create {}: {e}", parent.display()));
    }
    let journal: Arc<dyn Journal> = match SqliteJournal::open(&journal_path, SystemClock) {
        Ok(j) => Arc::new(j),
        Err(e) => {
            return soft_pair(&format!(
                "could not open the journal at {}: {e}",
                journal_path.display()
            ));
        }
    };

    // 3. Derive identity and construct the manager over the SAME journal.
    let entrypoint = format!("cmd:{}", options.flow);
    let args_h = args_hash(&options.command);
    let step_key = StepKey::new(format!("{entrypoint}#{args_h}"));
    let desc = FlowDescriptor {
        entrypoint: entrypoint.clone(),
        args_hash: args_h,
        explicit_key: options.flow_id.clone(),
        code_hash: Some(code_hash(&options.command)),
    };
    let flow_id = desc.flow_id();

    let started_ms = SystemClock.now_ms();
    let holder = holder_string(&hostname(), std::process::id(), started_ms);
    let engine = Arc::new(Engine::new());
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let manager = FlowManager::new(engine, Arc::clone(&journal), clock, ProcessId::new(holder));

    // 4. Pre-entry side-effect gate (KEEL-E033).
    if let Some(refusal) = side_effect_gate(
        journal.as_ref(),
        &flow_id,
        &entrypoint,
        &options.journal_files,
        options.force,
    ) {
        return refusal;
    }

    // 5. Enter (with dead-PID abandonment / on_busy retry loop).
    let mut handle = match enter_loop(&manager, journal.as_ref(), project, &desc, &flow_id) {
        Ok(handle) => handle,
        Err(terminal) => return terminal,
    };

    // 6. Completed flow -> pure replay-skip: do NOT spawn.
    if handle.is_replay_only() {
        let code = recorded_exit(journal.as_ref(), &flow_id);
        let report = ExecReport {
            exit_code: code,
            flow_id: flow_id.to_string(),
            entrypoint,
            replayed: true,
            skipped: false,
            forced: options.force,
            journal_files: snapshot_files(&options.journal_files),
        };
        let human = format!(
            "keel \u{25b8} exec: flow {flow_id} already completed \u{2014} replaying recorded \
             outcome (exit {code}); the command is NOT re-run.\n"
        );
        return (Some(report.render(human, false)), code);
    }

    // 7. Live run.
    let result = live_run(journal.as_ref(), &flow_id, &step_key, options);
    let completion = if result.exit_code == 0 && !result.spawn_failed {
        handle.complete_success()
    } else {
        handle.complete_failed()
    };
    if let Err(e) = completion {
        eprintln!("keel \u{25b8} exec: flow {flow_id} terminal status not journaled: {e}");
    }
    let code = result.exit_code;
    let report = ExecReport {
        exit_code: code,
        flow_id: flow_id.to_string(),
        entrypoint,
        replayed: false,
        skipped: false,
        forced: options.force,
        journal_files: snapshot_files(&options.journal_files),
    };
    let human = format!("keel \u{25b8} exec: flow {flow_id} exited {code}.\n");
    (Some(report.render(human, code != EXIT_OK)), code)
}

/// The pre-entry side-effect gate (KEEL-E033). `Some(terminal)` refuses the
/// re-dispatch; `None` proceeds. Only a `running`/`failed` flow with a recorded
/// pre-run snapshot is gated; `--force` prints a loud note and proceeds.
fn side_effect_gate(
    journal: &dyn Journal,
    flow_id: &FlowId,
    entrypoint: &str,
    journal_files: &[PathBuf],
    force: bool,
) -> Option<(Option<Rendered>, i32)> {
    let flow = journal.get_flow(flow_id).ok().flatten()?;
    if !matches!(flow.status, FlowStatus::Running | FlowStatus::Failed) {
        return None;
    }
    let (_, marker) = journal.step_at(flow_id, SNAP_BEFORE_SEQ).ok().flatten()?;
    let recorded: Vec<FileSnap> = marker
        .payload
        .as_deref()
        .and_then(decode_payload)
        .and_then(|v| v.get("files").cloned())
        .and_then(|f| serde_json::from_value(f).ok())
        .unwrap_or_default();
    let current = snapshot_files(journal_files);
    let changed = changed_files(&recorded, &current);
    if changed.is_empty() {
        return None;
    }
    if force {
        eprintln!(
            "keel \u{25b8} exec --force: {} declared side-effect file(s) changed since the last \
             attempt ({}); re-dispatching anyway (KEEL-E033 overridden).",
            changed.len(),
            changed.join(", ")
        );
        return None;
    }
    let message = format!(
        "refusing to re-dispatch flow {flow_id} ({entrypoint}): {} declared side-effect file(s) \
         changed since the last attempt ({}). The previous run left partial effects; re-running \
         could duplicate them. Re-run with --force to override (KEEL-E033); see `keel explain \
         KEEL-E033`.",
        changed.len(),
        changed.join(", ")
    );
    Some(soft_pair(&message))
}

/// Enter the flow, handling KEEL-E030 (dead-PID abandonment or `on_busy`) and
/// KEEL-E032 (dead flow). `Ok(handle)` is a live-or-replay handle; `Err` is a
/// terminal `(rendered, code)` to return from [`run`].
fn enter_loop(
    manager: &FlowManager,
    journal: &dyn Journal,
    project: &Path,
    desc: &FlowDescriptor,
    flow_id: &FlowId,
) -> Result<FlowHandle, (Option<Rendered>, i32)> {
    let mut wait_iters: u32 = 0;
    loop {
        match manager.enter_flow(desc) {
            Ok(handle) => return Ok(handle),
            Err(e) => match e.code {
                ErrorCode::FlowLeaseHeld => {
                    if let Some(terminal) =
                        handle_busy(journal, project, flow_id, &e.message, &mut wait_iters)
                    {
                        return Err(terminal);
                    }
                    // Retry: an abandoned dead holder, or an `on_busy = wait`
                    // sleep, has cleared the way.
                }
                ErrorCode::FlowDead => {
                    return Err(soft_pair(&format!(
                        "{} Inspect with `keel trace {flow_id}`; see `keel explain KEEL-E032`.",
                        e.message
                    )));
                }
                _ => {
                    return Err(soft_pair(&format!(
                        "could not enter flow {flow_id}: {}",
                        e.message
                    )));
                }
            },
        }
    }
}

/// How many `wait`-mode iterations ([`Duration::from_millis`]`(500)` apart)
/// between "still waiting" safety prints — `60 * 500ms = 30s`. No timeout in
/// v1 (the operator can ^C); this print is the only feedback a long wait
/// gives, so it must actually recur, not fire once and go silent.
const WAIT_NOTICE_EVERY: u32 = 60;

/// Handle a held lease: abandon a dead same-host holder (then retry), else
/// apply `[flows].on_busy`. `Some(terminal)` ends the run; `None` retries.
/// `wait_iters` threads the `wait`-mode iteration count across calls (one
/// call per [`enter_loop`] retry) so the safety print fires on a cadence
/// instead of every 500ms or never.
fn handle_busy(
    journal: &dyn Journal,
    project: &Path,
    flow_id: &FlowId,
    e030_message: &str,
    wait_iters: &mut u32,
) -> Option<(Option<Rendered>, i32)> {
    let recorded_holder = journal
        .get_flow(flow_id)
        .ok()
        .flatten()
        .and_then(|f| f.lease_holder);
    if let Some(holder) = &recorded_holder
        && let Some(parsed) = parse_holder(holder.as_str())
        && parsed.host == hostname()
        && pid_is_dead(parsed.pid)
    {
        eprintln!(
            "keel \u{25b8} exec: abandoning flow {flow_id} held by dead pid {} on this host.",
            parsed.pid
        );
        // complete_flow(Failed) clears the lease; the re-entry consumes an
        // attempt (cap -> Dead/KEEL-E032, unchanged).
        if let Err(err) = journal.complete_flow(flow_id, FlowStatus::Failed) {
            eprintln!("keel \u{25b8} exec: could not abandon dead-held flow {flow_id}: {err}");
        }
        return None;
    }

    match flows_on_busy(project) {
        OnBusy::Skip => {
            let report = ExecReport {
                exit_code: EXIT_OK,
                flow_id: flow_id.to_string(),
                entrypoint: String::new(),
                replayed: false,
                skipped: true,
                forced: false,
                journal_files: Vec::new(),
            };
            let human = format!(
                "keel \u{25b8} exec: flow {flow_id} is busy (held by a live process); skipping \
                 (flows.on_busy = skip).\n"
            );
            Some((Some(report.render(human, false)), EXIT_OK))
        }
        OnBusy::Wait => {
            *wait_iters += 1;
            if (*wait_iters).is_multiple_of(WAIT_NOTICE_EVERY) {
                let holder = recorded_holder
                    .as_ref()
                    .map_or("unknown", ProcessId::as_str);
                eprintln!(
                    "keel \u{25b8} exec: still waiting on flow {flow_id} held by {holder} \
                     ({}s elapsed; flows.on_busy = wait; ^C to give up).",
                    (*wait_iters / 2) // 500ms per iteration -> iters/2 = seconds
                );
            }
            std::thread::sleep(Duration::from_millis(500));
            None
        }
        OnBusy::Fail => Some(soft_pair(&format!(
            "{e030_message} (flows.on_busy = fail); see `keel explain KEEL-E030`."
        ))),
    }
}

/// The effective `[flows].on_busy` from the project's `keel.toml` (default
/// `skip`), via the CLI's one shared `keel.toml`\u{2192}[`Policy`] loader
/// ([`evidence::load_policy`] — the same pipeline `resolved_journal` reads).
/// Lenient: any read/parse failure applies the default.
fn flows_on_busy(project: &Path) -> OnBusy {
    evidence::load_policy(project)
        .and_then(|p| p.flows)
        .map_or_else(OnBusy::default, |f| f.on_busy)
}

/// The recorded exit code for a completed flow's single command step: 0 for an
/// `ok` record, else the recorded payload's `exit_code`.
fn recorded_exit(journal: &dyn Journal, flow_id: &FlowId) -> i32 {
    match journal.step_at(flow_id, STEP_SEQ) {
        Ok(Some((_, outcome))) if outcome.status == StepStatus::Ok => EXIT_OK,
        Ok(Some((_, outcome))) => outcome
            .payload
            .as_deref()
            .and_then(decode_payload)
            .and_then(|v| v.get("exit_code").and_then(Value::as_i64))
            .and_then(|c| i32::try_from(c).ok())
            .unwrap_or(EXIT_FAILURE),
        _ => EXIT_OK,
    }
}

/// The live path: record the before snapshot marker, the `running` step, spawn
/// and tee the child, record the terminal step and the after snapshot marker.
/// All journal-write failures degrade to a stderr warning (resilience first).
fn live_run(
    journal: &dyn Journal,
    flow_id: &FlowId,
    step_key: &StepKey,
    options: &ExecOptions,
) -> SpawnResult {
    let program = resolve_program(&options.command[0]);
    let before = json!({
        "files": snapshot_files(&options.journal_files),
        "argv": options.command,
        "program": program,
    });
    record_marker(
        journal,
        flow_id,
        SNAP_BEFORE_SEQ,
        "cmd:snapshot:before",
        &before,
    );

    let start = SystemClock.now_ms();
    record_step(
        journal,
        flow_id,
        step_key,
        &StepOutcome {
            kind: StepKind::Subprocess,
            attempt: 0,
            status: StepStatus::Running,
            payload: None,
            error_class: None,
            started_at: start,
            ended_at: None,
        },
    );

    let result = run_child(&options.command);

    let end = SystemClock.now_ms();
    let (status, error_class) = if result.exit_code == 0 && !result.spawn_failed {
        (StepStatus::Ok, None)
    } else {
        (StepStatus::Error, Some(ErrorClass::Other))
    };
    let terminal_payload = json!({
        "exit_code": result.exit_code,
        "stdout_tail": result.stdout_tail,
        "stderr_tail": result.stderr_tail,
    });
    record_step(
        journal,
        flow_id,
        step_key,
        &StepOutcome {
            kind: StepKind::Subprocess,
            attempt: 1,
            status,
            payload: encode_payload(&terminal_payload),
            error_class,
            started_at: start,
            ended_at: Some(end),
        },
    );

    let after = json!({
        "files": snapshot_files(&options.journal_files),
        "argv": options.command,
        "program": program,
    });
    record_marker(
        journal,
        flow_id,
        SNAP_AFTER_SEQ,
        "cmd:snapshot:after",
        &after,
    );

    result
}

/// Record a reserved-lane marker, degrading a journal failure to a warning.
fn record_marker(journal: &dyn Journal, flow_id: &FlowId, seq: u64, key: &str, payload: &Value) {
    let now = SystemClock.now_ms();
    let outcome = StepOutcome {
        kind: StepKind::Marker,
        attempt: 0,
        status: StepStatus::Ok,
        payload: encode_payload(payload),
        error_class: None,
        started_at: now,
        ended_at: Some(now),
    };
    if let Err(e) = journal.record_step(flow_id, seq, &StepKey::new(key), &outcome) {
        eprintln!("keel \u{25b8} exec: {key} marker not journaled: {e}");
    }
}

/// Record the command step, degrading a journal failure to a warning (a lost
/// record costs replay dedup, never correctness — the `running` marker's
/// absence just makes a resume re-run the command).
fn record_step(journal: &dyn Journal, flow_id: &FlowId, key: &StepKey, outcome: &StepOutcome) {
    if let Err(e) = journal.record_step(flow_id, STEP_SEQ, key, outcome) {
        eprintln!("keel \u{25b8} exec: step {STEP_SEQ} not journaled: {e}");
    }
}

// ---- test support ----------------------------------------------------
//
// `tests/exec.rs` seeds journal rows directly (the same technique
// `resume`-style tests use to simulate a foreign holder) to exercise
// on_busy/dead-PID paths without a second real process. A seeded row that
// does not share the EXACT `flow_id` `run` derives collides with nothing and
// the test passes vacuously, so the derivation (entrypoint/args_hash
// composition, the holder-string format) is exposed here rather than
// re-implemented (and silently drifted) in the test file.

/// The `flow_id` [`run`] derives for `(flow, command, flow_id_key)` — for
/// integration tests seeding a colliding row.
#[doc(hidden)]
#[must_use]
pub fn identity_flow_id(flow: &str, command: &[String], flow_id_key: Option<&str>) -> String {
    FlowDescriptor {
        entrypoint: format!("cmd:{flow}"),
        args_hash: args_hash(command),
        explicit_key: flow_id_key.map(str::to_owned),
        code_hash: None,
    }
    .flow_id()
    .to_string()
}

/// This host's name, as [`run`] records it in a lease holder — for
/// integration tests constructing a same-host (or foreign-host) holder.
#[doc(hidden)]
#[must_use]
pub fn identity_hostname() -> String {
    hostname()
}

/// The `"{host}:{pid}:{started_ms}"` lease holder format [`run`] writes — for
/// integration tests seeding a foreign holder.
#[doc(hidden)]
#[must_use]
pub fn identity_holder_string(host: &str, pid: u32, started_ms: i64) -> String {
    holder_string(host, pid, started_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flow_name_grammar_is_enforced() {
        assert!(valid_flow_name("autonomous-run"));
        assert!(valid_flow_name("a1"));
        assert!(!valid_flow_name("Autonomous"));
        assert!(!valid_flow_name("-x"));
        assert!(!valid_flow_name(""));
        assert!(!valid_flow_name("a_b"));
    }

    #[test]
    fn identity_is_deterministic_and_argv_sensitive() {
        let a = args_hash(&["uvx".into(), "server".into()]);
        let b = args_hash(&["uvx".into(), "server".into()]);
        let c = args_hash(&["uvx".into(), "other".into()]);
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 16);
    }

    #[test]
    fn holder_round_trips_and_detects_shape() {
        let h = holder_string("myhost", 4242, 1_783_728_000_000);
        let parsed = parse_holder(&h).unwrap();
        assert_eq!(parsed.host, "myhost");
        assert_eq!(parsed.pid, 4242);
        assert!(
            parse_holder("host-a:pid-1").is_none(),
            "legacy holders don't parse"
        );
    }

    #[test]
    fn snapshot_compare_flags_growth_change_and_missing() {
        let dir = tempfile::TempDir::new().unwrap();
        let f = dir.path().join("trades.jsonl");
        std::fs::write(&f, "a\nb\n").unwrap();
        let one = std::slice::from_ref(&f);
        let before = snapshot_files(one);
        assert!(changed_files(&before, &snapshot_files(one)).is_empty());
        std::fs::write(&f, "a\nb\nc\n").unwrap();
        assert_eq!(
            changed_files(&before, &snapshot_files(one)),
            vec![f.display().to_string()]
        );
        std::fs::remove_file(&f).unwrap();
        assert_eq!(changed_files(&before, &snapshot_files(one)).len(), 1);
    }

    #[test]
    fn payload_codec_round_trips_and_reads_bare() {
        let value = json!({ "exit_code": 3, "stdout_tail": "hi" });
        let bytes = encode_payload(&value).expect("encodes");
        assert_eq!(decode_payload(&bytes), Some(value));
    }
}
