//! Tier 1 live event sink: an append-only NDJSON feed of what the engine is
//! doing *right now* — attempt starts/failures, backoff waits, breaker
//! open/half-open/close, rate-limit queueing, cache hits/misses — written
//! per run to `.keel/events/<run>.ndjson` for `keel tail` / `keel trace` to
//! follow (dx-spec §6, invariant 4). **Non-contract**: the line format is
//! versioned (`"v": 1` on every line) but lives outside `contracts/`; only
//! Keel's own tooling reads it, and no daemon is involved — readers tail the
//! file (dx invariant 3).
//!
//! # Activation (the decision, documented)
//!
//! Resolved once per [`Engine::new`](crate::Engine::new) from the process
//! environment ([`EventSink::from_env`], testable as [`resolve_events_dir`],
//! which returns an [`EventDestination`]):
//!
//! - `KEEL_EVENTS` set to `0` / `false` / `off` / empty — force **off**.
//! - `KEEL_EVENTS` set to `stderr` (case-insensitive) — every line goes to
//!   the process's **stderr** instead of a file; `./.keel/events/` is never
//!   created. On Cloud Run, GKE and Lambda stderr already **is** the log
//!   pipeline, so this is the whole remote-evidence story for a 12-factor
//!   deploy — no collector, no volume, no custom wheel (partial step toward
//!   #94; the real remote-sink design stays open). Native core only: the
//!   stub backends have no event sink to redirect.
//! - `KEEL_EVENTS` set to anything else (`1`, `true`, …) — force **on**
//!   (creates `./.keel/events/` on demand).
//! - unset — **on** exactly when `./.keel` already exists (a Keel-initialized
//!   project directory), **off** otherwise. `./.keel` is rooted at `KEEL_CWD`
//!   when set, else the process working directory.
//!
//! Off is a zero-cost no-op: the engine holds no sink and every emit site is
//! one `Option` discriminant check (the overhead bench's `a_empty` /
//! `b_cache_miss` / `c_cache_hit` cases run this path; `d_events` measures
//! the on path). A sink that cannot open (unwritable directory, or — in
//! principle — a broken stderr fd) degrades to a `warn!` and off —
//! observability never fails the wrapped call.
//!
//! ## The stderr sink is deliberately unbuffered
//!
//! The file sink wraps its `File` in [`io::BufWriter`] (see
//! [`EventSink::open`]) because the writer thread already flushes whenever
//! its queue drains, so the extra userspace buffer only saves syscalls
//! between drains. The stderr sink ([`EventSink::open_stderr`]) does **not**
//! wrap [`io::stderr`] in a `BufWriter`: stderr exists to be captured
//! mid-stream by whatever is watching the process (a platform's log
//! pipeline), and the entire point of this destination is that evidence
//! survives an instance that dies mid-run. An extra buffer that only empties
//! on drain-or-shutdown is exactly the mechanism that would make the last
//! few events vanish with the container instead of reaching the log
//! pipeline.
//!
//! [`write_events`] serializes each event plus its trailing newline into one
//! in-memory buffer *before* touching `out`, then issues exactly **one**
//! `write_all` per event — never a separate call for the JSON body and the
//! newline. That single call is what makes "no explicit flush required for
//! durability" true: with `io::Stderr`'s lack of internal buffering (unlike
//! `io::Stdout`), one `write_all` reaches the OS as one `write(2)`. What this
//! does **not** buy: a single `write(2)` is not a guaranteed-atomic unit on
//! every platform or stream type, so a line can still interleave with
//! concurrent writers of the *same* fd (Keel's own console output, or the
//! host application's own direct `stderr` writes) under extreme conditions —
//! e.g. a line wider than the OS pipe buffer, or a destination that isn't a
//! pipe/regular file. What holds: on the common case (a line that fits in one
//! `write(2)`, going to a pipe or regular file — true for essentially every
//! real event line on a real deployment target), POSIX guarantees that write
//! is atomic with respect to other writers of the same fd, so this is as
//! close to atomic as a userspace program gets without its own external
//! locking. An explicit flush is still only ever about a *reader* (e.g. a
//! same-process `keel trace`) seeing the file's tail promptly, not about
//! durability.
//!
//! ## Interleaving with Keel's other stderr output
//!
//! `KEEL_EVENTS=stderr` shares the stream with Keel's activation banner,
//! exit summary, and warning/error lines — this module does not coordinate
//! with them (it has no knowledge of `KEEL_LOG_FORMAT`, which is a front-end
//! concern). Every event line is a bare NDJSON object (`{"v":1,"seq":...}`,
//! no envelope beyond the format documented above) whether or not
//! `KEEL_LOG_FORMAT=json` is set; the two are structurally distinguishable
//! (event lines are recognizable by their fixed `v`/`seq`/`ms`/`event` keys)
//! but a consumer wanting to tell them apart reliably should route them to
//! separate destinations (e.g. `2>events.ndjson` isn't available on a
//! platform that only captures one stderr stream, but a structured-log
//! collector can still filter on the `event` key's presence). This is a
//! known limitation of shipping evidence and console output on one stream —
//! not solved here, and out of scope for this partial step toward #94.
//!
//! # Hot-path budget (dx invariant 8: ≤10µs)
//!
//! [`EventSink::emit`] allocates the event, stamps `seq`, and hands it to a
//! background writer thread over a channel; JSON serialization and file I/O
//! never run on the wrapped call's path. The writer buffers and flushes
//! whenever its queue drains, so a live `keel tail` sees events promptly
//! without a flush syscall per line.
//!
//! # Ordering and time
//!
//! `seq` is per-run monotonic and equals physical line order (allocated under
//! the same lock that submits to the writer, so no interleaving can reorder
//! the file). `ms` is engine-elapsed milliseconds from the engine's tokio
//! clock — virtual under `start_paused`, so tests are wall-clock free. Wall
//! time appears exactly once, in the `run_start` header line of a production
//! sink (never under [`EventSink::to_writer`], the deterministic test/bench
//! constructor).
//!
//! # Trace refs
//!
//! Every call's first event is `call_start`; the run id plus that event's
//! `seq` form the [`TraceRef`] (`<run>#<seq>`) the engine appends to Tier 1
//! terminal failure messages (`… trace: keel trace <ref>`, dx invariant 4) —
//! only while a sink is active, so every implementation stays
//! message-identical under conformance conditions (no sink attached). To
//! resolve a ref: parse it ([`TraceRef::from_str`]), open
//! `.keel/events/<run>.ndjson` ([`TraceRef::file_name`]), find the line with
//! `seq` (the `call_start`), and select the call's other events by that
//! line's `call` id.

use core::fmt;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Mutex;
use std::sync::mpsc::{Receiver, Sender, SyncSender, TryRecvError, channel, sync_channel};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use keel_core_api::{ErrorClass, ErrorCode};
use serde::{Deserialize, Serialize};
use tracing::warn;

/// Sink line-format version — the `v` stamped on every line.
pub const EVENTS_VERSION: u32 = 1;

/// The subdirectory of `.keel/` holding per-run event files.
pub const EVENTS_SUBDIR: &str = "events";

/// File extension of a run's event file (newline-delimited JSON).
pub const EVENTS_EXT: &str = "ndjson";

/// How long [`EventSink::flush`] waits for the writer thread before giving
/// up — a wedged filesystem must never hang the caller.
const FLUSH_TIMEOUT: Duration = Duration::from_secs(5);

/// One NDJSON line: the envelope every event shares, with the event-specific
/// payload flattened beside it. Field order is fixed by this struct, so a
/// given event serializes byte-identically everywhere.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// Line-format version ([`EVENTS_VERSION`]).
    pub v: u32,
    /// Per-run monotonic sequence number; equals physical line order.
    pub seq: u64,
    /// Engine-elapsed milliseconds (virtual-clock-safe; never wall time).
    pub ms: u64,
    /// What happened, tagged as `"event"` in the JSON.
    #[serde(flatten)]
    pub kind: EventKind,
}

/// Which cache backend served (or missed) a call — mirrors the engine's
/// cache-plan split, not the policy's `scope` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheStore {
    /// The in-process map.
    Memory,
    /// The journal's `cache` table.
    Persistent,
}

/// The event vocabulary, tagged `"event"` with `snake_case` names. `call` is
/// the call's `trace_id` (the `Outcome` field), so one call's events can be
/// selected out of an interleaved feed; `target` repeats on every event so a
/// tail can filter without joining back to `call_start`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum EventKind {
    /// Per-run header, always `seq` 0: names the run and (production sinks
    /// only) anchors it to wall time and a pid.
    RunStart {
        /// The run id — also the event file's stem.
        run: String,
        /// Milliseconds since the Unix epoch at sink open; absent under the
        /// deterministic test/bench constructor.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        wall_ms: Option<u64>,
        /// The emitting process id; absent under the test/bench constructor.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pid: Option<u32>,
    },
    /// A call entered the layer chain (every call's first event; its `seq`
    /// is the [`TraceRef`] anchor).
    CallStart {
        call: String,
        target: String,
        op: String,
    },
    /// The cache served the call (attempts stays 0).
    CacheHit {
        call: String,
        target: String,
        scope: CacheStore,
    },
    /// A cache plan existed but held no fresh entry; the call runs live.
    CacheMiss {
        call: String,
        target: String,
        scope: CacheStore,
    },
    /// The rate limiter queued the call for `wait_ms` (emitted before the
    /// wait begins, so a live tail shows the queueing as it happens).
    Throttle {
        call: String,
        target: String,
        wait_ms: u64,
    },
    /// An open breaker failed the call fast (KEEL-E012; the effect never ran).
    BreakerReject { call: String, target: String },
    /// Cooldown elapsed: this call is the breaker's half-open probe.
    BreakerHalfOpen { call: String, target: String },
    /// The breaker tripped open (threshold reached, or the probe failed).
    BreakerOpen {
        call: String,
        target: String,
        cooldown_ms: u64,
    },
    /// A successful probe closed a previously-open breaker.
    BreakerClose { call: String, target: String },
    /// Attempt `attempt` (1-based) is about to invoke the effect.
    AttemptStart {
        call: String,
        target: String,
        attempt: u32,
    },
    /// Attempt `attempt` failed with the given class (pre-retry-decision).
    AttemptError {
        call: String,
        target: String,
        attempt: u32,
        class: ErrorClass,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        http_status: Option<u16>,
    },
    /// The retry layer is waiting `wait_ms` before the next attempt (emitted
    /// before the wait begins).
    Backoff {
        call: String,
        target: String,
        attempt: u32,
        wait_ms: u64,
    },
    /// The call settled (every call's last event). `result` mirrors the
    /// Outcome's `"ok"` / `"error"`; `code` is the terminal error code.
    CallEnd {
        call: String,
        target: String,
        result: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<ErrorCode>,
        attempts: u32,
    },
}

/// A stable reference to one call in one run's event feed: the run id plus
/// the `seq` of the call's `call_start` line. Rendered `<run>#<seq>` — the
/// token Tier 1 failure messages carry after `trace: keel trace`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceRef {
    /// The run id (the event file's stem).
    pub run: String,
    /// The `seq` of the call's `call_start` event.
    pub seq: u64,
}

impl TraceRef {
    /// The event file this ref resolves against, relative to `.keel/events/`.
    #[must_use]
    pub fn file_name(&self) -> String {
        format!("{}.{EVENTS_EXT}", self.run)
    }
}

impl fmt::Display for TraceRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}#{}", self.run, self.seq)
    }
}

/// A string that failed to parse as a [`TraceRef`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseTraceRefError;

impl fmt::Display for ParseTraceRefError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("trace ref must look like <run>#<seq>, e.g. 019a2b3c4d5-7f2e#12")
    }
}

impl std::error::Error for ParseTraceRefError {}

impl FromStr for TraceRef {
    type Err = ParseTraceRefError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (run, seq) = s.rsplit_once('#').ok_or(ParseTraceRefError)?;
        if run.is_empty() {
            return Err(ParseTraceRefError);
        }
        let seq = seq.parse().map_err(|_| ParseTraceRefError)?;
        Ok(Self {
            run: run.to_owned(),
            seq,
        })
    }
}

/// The environment inputs the activation decision depends on, captured as
/// plain data so [`resolve_events_dir`] is unit-testable without touching
/// process globals.
#[derive(Debug, Clone)]
pub struct EventsEnv {
    /// The value of `KEEL_EVENTS`, if set.
    pub keel_events: Option<String>,
    /// The directory whose `.keel/` marks a Keel-initialized project
    /// (`KEEL_CWD` when set, else the process working directory).
    pub base_dir: PathBuf,
}

impl EventsEnv {
    /// Snapshot the real process environment.
    #[must_use]
    pub fn capture() -> Self {
        Self::capture_from(
            |k| std::env::var(k).ok(),
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        )
    }

    /// The testable core of [`capture`](Self::capture): `KEEL_CWD` (set,
    /// non-blank, a directory) is the config root discovery and the journal
    /// already use, so the event feed keys on it too — a `KEEL_CWD`-relocated
    /// process must not end up with discovery on and events off (WS8 probe,
    /// #92).
    pub fn capture_from(var: impl Fn(&str) -> Option<String>, cwd: PathBuf) -> Self {
        let base_dir = var("KEEL_CWD")
            .map(|v| v.trim().to_owned())
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .filter(|p| p.is_dir())
            .unwrap_or(cwd);
        Self {
            keel_events: var("KEEL_EVENTS"),
            base_dir,
        }
    }
}

/// Where the event feed should go — off, a per-run file under a directory,
/// or the process's stderr. See the module docs for the decision table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventDestination {
    /// No sink; the engine holds `None`.
    Off,
    /// File-backed: a fresh `<run>.ndjson` is created under this directory.
    Dir(PathBuf),
    /// Every line goes to the process's stderr (`KEEL_EVENTS=stderr`).
    Stderr,
}

/// Where events should be written. See the module docs for the decision
/// table this implements.
#[must_use]
pub fn resolve_events_dir(env: &EventsEnv) -> EventDestination {
    let keel_dir = env.base_dir.join(".keel");
    match env.keel_events.as_deref().map(str::trim) {
        Some(v)
            if v.is_empty()
                || v.eq_ignore_ascii_case("0")
                || v.eq_ignore_ascii_case("false")
                || v.eq_ignore_ascii_case("off") =>
        {
            EventDestination::Off
        }
        Some(v) if v.eq_ignore_ascii_case("stderr") => EventDestination::Stderr,
        Some(_) => EventDestination::Dir(keel_dir.join(EVENTS_SUBDIR)),
        None if keel_dir.is_dir() => EventDestination::Dir(keel_dir.join(EVENTS_SUBDIR)),
        None => EventDestination::Off,
    }
}

/// What crosses to the writer thread. Events stay typed until the writer
/// serializes them — serialization is never on the wrapped call's path.
enum Msg {
    Event(Event),
    Flush(SyncSender<()>),
    Shutdown,
}

/// Sequence allocation and submission, under one lock so `seq` order and
/// physical line order can never diverge.
#[derive(Debug)]
struct Emitter {
    seq: u64,
    tx: Sender<Msg>,
}

/// The live event sink: run identity + a buffered background NDJSON writer.
/// One per engine; `&self`-concurrent (emit locks only to stamp `seq` and
/// enqueue). Dropping the sink drains and flushes the feed (the writer thread
/// is joined), so an engine dropped at process end loses nothing.
#[derive(Debug)]
pub struct EventSink {
    emitter: Mutex<Emitter>,
    run_id: String,
    path: Option<PathBuf>,
    writer: Option<JoinHandle<()>>,
}

impl EventSink {
    /// Resolve activation from the process environment (see module docs) and
    /// open the resolved destination. `None` when off — or when the sink
    /// cannot open, which degrades to a `warn!` (observability never fails
    /// the wrapped call).
    #[must_use]
    pub fn from_env() -> Option<Self> {
        match resolve_events_dir(&EventsEnv::capture()) {
            EventDestination::Off => None,
            EventDestination::Stderr => match Self::open_stderr() {
                Ok(sink) => Some(sink),
                Err(error) => {
                    warn!(error = %error, "stderr event sink unavailable; live events disabled");
                    None
                }
            },
            EventDestination::Dir(dir) => match Self::open(&dir) {
                Ok(sink) => Some(sink),
                Err(error) => {
                    warn!(dir = %dir.display(), error = %error, "event sink unavailable; live events disabled");
                    None
                }
            },
        }
    }

    /// Open a production sink in `dir` (created on demand): a fresh run id, a
    /// `<run>.ndjson` file, and a `run_start` header anchored to wall time.
    pub fn open(dir: &Path) -> io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        // create_new: a run-id collision must never clobber another process's
        // feed. Retry under a fresh id — the random suffix makes repeated
        // same-millisecond collisions vanishingly unlikely.
        let mut collision: io::Error = io::ErrorKind::AlreadyExists.into();
        for _ in 0..3 {
            let run_id = new_run_id();
            let path = dir.join(format!("{run_id}.{EVENTS_EXT}"));
            match std::fs::File::create_new(&path) {
                Ok(file) => {
                    return Self::start(
                        Box::new(io::BufWriter::new(file)),
                        run_id,
                        Some(path),
                        Some(epoch_ms()),
                        Some(std::process::id()),
                    );
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => collision = e,
                Err(e) => return Err(e),
            }
        }
        Err(collision)
    }

    /// Open a production sink that writes to the process's stderr instead of
    /// a file: a fresh run id and a `run_start` header anchored to wall time,
    /// same as [`Self::open`], but no file, no directory, and — see the
    /// module docs — deliberately no [`io::BufWriter`] around the writer.
    pub fn open_stderr() -> io::Result<Self> {
        Self::start(
            Box::new(io::stderr()),
            new_run_id(),
            None,
            Some(epoch_ms()),
            Some(std::process::id()),
        )
    }

    /// Deterministic test/bench sink: write to any `Write` under a caller-fixed
    /// run id, with no wall-clock or pid fields anywhere in the feed — the
    /// same events always produce byte-identical output.
    pub fn to_writer(writer: Box<dyn Write + Send>, run_id: &str) -> io::Result<Self> {
        Self::start(writer, run_id.to_owned(), None, None, None)
    }

    fn start(
        writer: Box<dyn Write + Send>,
        run_id: String,
        path: Option<PathBuf>,
        wall_ms: Option<u64>,
        pid: Option<u32>,
    ) -> io::Result<Self> {
        let (tx, rx) = channel();
        let handle = std::thread::Builder::new()
            .name("keel-events".to_owned())
            .spawn(move || write_events(&rx, writer))?;
        let sink = Self {
            emitter: Mutex::new(Emitter { seq: 0, tx }),
            run_id: run_id.clone(),
            path,
            writer: Some(handle),
        };
        sink.emit(
            0,
            EventKind::RunStart {
                run: run_id,
                wall_ms,
                pid,
            },
        );
        Ok(sink)
    }

    /// This run's id — the token trace refs and the event file name carry.
    #[must_use]
    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// The event file being written, if this is a file-backed sink.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Stamp `kind` with the next `seq` and the caller's clock reading, and
    /// hand it to the writer. Returns the assigned `seq` (the [`TraceRef`]
    /// anchor for `call_start`). A dead writer degrades to a dropped event.
    pub fn emit(&self, ms: u64, kind: EventKind) -> u64 {
        let mut emitter = self.emitter.lock().expect("event sink lock poisoned");
        let seq = emitter.seq;
        emitter.seq += 1;
        let _ = emitter.tx.send(Msg::Event(Event {
            v: EVENTS_VERSION,
            seq,
            ms,
            kind,
        }));
        seq
    }

    /// Block until every event emitted so far is written and flushed (bounded
    /// by [`FLUSH_TIMEOUT`]). For readers — tests, a same-process `keel
    /// trace` — that need the file current *now*; the writer also flushes on
    /// its own whenever its queue drains.
    pub fn flush(&self) {
        let (ack_tx, ack_rx) = sync_channel(1);
        {
            let emitter = self.emitter.lock().expect("event sink lock poisoned");
            if emitter.tx.send(Msg::Flush(ack_tx)).is_err() {
                return; // writer already gone; nothing left to flush
            }
        }
        let _ = ack_rx.recv_timeout(FLUSH_TIMEOUT);
    }
}

impl Drop for EventSink {
    fn drop(&mut self) {
        // `get_mut`: exclusive access, so a poisoned lock cannot block the
        // drain. The writer flushes everything queued before honoring the
        // shutdown, then the join guarantees the file is complete.
        if let Ok(emitter) = self.emitter.get_mut() {
            let _ = emitter.tx.send(Msg::Shutdown);
        }
        if let Some(handle) = self.writer.take() {
            let _ = handle.join();
        }
    }
}

/// The writer thread: serialize + buffer each event, flush whenever the queue
/// drains (so a tail sees events promptly, without a flush syscall per line).
/// Write failures drop lines, never the call. `Shutdown` still drains what is
/// already queued — messages ahead of it in the channel are processed first.
///
/// Each event is serialized into a scratch buffer first and reaches `out`
/// through exactly **one** `write_all` call (JSON body + trailing newline
/// together) — never two separate calls. Two calls would mean two raw
/// `write(2)`s per event on an unbuffered destination (stderr), and nothing
/// stops the file's own bytes and a newline from landing on either side of
/// unrelated output the same fd receives between them (Keel's own console
/// lines, or the host application's own writes) — corrupting the very line
/// this sink exists to make trustworthy. One `write_all` is not a portable
/// atomicity guarantee in every case (see the module docs), but it is the
/// most any userspace writer can do without its own external locking, and it
/// is exact for the common case that matters here.
fn write_events(rx: &Receiver<Msg>, mut out: Box<dyn Write + Send>) {
    let mut dirty = false;
    let mut line = Vec::with_capacity(256);
    loop {
        let msg = if dirty {
            match rx.try_recv() {
                Ok(msg) => msg,
                Err(TryRecvError::Empty) => {
                    let _ = out.flush();
                    dirty = false;
                    continue;
                }
                Err(TryRecvError::Disconnected) => break,
            }
        } else {
            match rx.recv() {
                Ok(msg) => msg,
                Err(_) => break,
            }
        };
        match msg {
            Msg::Event(event) => {
                line.clear();
                if serde_json::to_writer(&mut line, &event).is_ok() {
                    line.push(b'\n');
                    if out.write_all(&line).is_ok() {
                        dirty = true;
                    }
                }
            }
            Msg::Flush(ack) => {
                let _ = out.flush();
                dirty = false;
                let _ = ack.send(());
            }
            Msg::Shutdown => break,
        }
    }
    let _ = out.flush();
}

/// A fresh run id: zero-padded hex epoch-milliseconds (lexically sortable, so
/// "latest run" is a name sort) plus a random suffix against same-ms
/// collisions. Wall clock is fine here — production names only; deterministic
/// tests fix the run id via [`EventSink::to_writer`].
fn new_run_id() -> String {
    format!("{:011x}-{:04x}", epoch_ms(), fastrand::u16(..))
}

fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

#[cfg(test)]
mod tests {
    use super::{
        EVENTS_SUBDIR, Event, EventDestination, EventKind, EventSink, EventsEnv,
        ParseTraceRefError, TraceRef, resolve_events_dir,
    };
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    /// A `Write` the test keeps a handle on after the sink takes the box.
    #[derive(Debug, Clone, Default)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl SharedBuf {
        fn contents(&self) -> String {
            String::from_utf8(self.0.lock().expect("buf lock").clone()).expect("utf-8 feed")
        }
    }

    impl Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("buf lock").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn env(keel_events: Option<&str>, base_dir: &std::path::Path) -> EventsEnv {
        EventsEnv {
            keel_events: keel_events.map(str::to_owned),
            base_dir: base_dir.to_owned(),
        }
    }

    #[test]
    fn capture_prefers_keel_cwd_when_it_is_a_directory() {
        let dir = tempfile::TempDir::new().unwrap();
        let keel_cwd = dir.path().to_string_lossy().into_owned();
        let env = |k: &str| (k == "KEEL_CWD").then(|| keel_cwd.clone());
        let got = EventsEnv::capture_from(env, PathBuf::from("/elsewhere"));
        assert_eq!(got.base_dir, dir.path());
        // blank or non-directory values fall back to cwd
        let blank = |k: &str| (k == "KEEL_CWD").then(|| "  ".to_owned());
        assert_eq!(
            EventsEnv::capture_from(blank, PathBuf::from("/elsewhere")).base_dir,
            PathBuf::from("/elsewhere")
        );
        let missing = |k: &str| (k == "KEEL_CWD").then(|| "/definitely/not/a/dir/keel".to_owned());
        assert_eq!(
            EventsEnv::capture_from(missing, PathBuf::from("/elsewhere")).base_dir,
            PathBuf::from("/elsewhere")
        );
        let none = |_: &str| None;
        assert_eq!(
            EventsEnv::capture_from(none, PathBuf::from("/elsewhere")).base_dir,
            PathBuf::from("/elsewhere")
        );
    }

    #[test]
    fn activation_decision_table() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let bare = tmp.path(); // no .keel yet

        // Unset + no .keel dir: off.
        assert_eq!(resolve_events_dir(&env(None, bare)), EventDestination::Off);
        // Explicitly off, in every accepted spelling, beats everything.
        for off in ["0", "false", "off", "FALSE", "Off", "", "  "] {
            assert_eq!(
                resolve_events_dir(&env(Some(off), bare)),
                EventDestination::Off,
                "{off:?}"
            );
        }
        // Any other set value forces on, .keel dir or not.
        let expected = bare.join(".keel").join(EVENTS_SUBDIR);
        for on in ["1", "true", "on", "yes"] {
            assert_eq!(
                resolve_events_dir(&env(Some(on), bare)),
                EventDestination::Dir(expected.clone()),
                "{on:?}"
            );
        }
        // Unset + an existing .keel dir: on (the keel-initialized project case).
        std::fs::create_dir(bare.join(".keel")).expect("mk .keel");
        assert_eq!(
            resolve_events_dir(&env(None, bare)),
            EventDestination::Dir(expected)
        );
        // A .keel FILE is not a project marker.
        let tmp2 = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp2.path().join(".keel"), b"not a dir").expect("write file");
        assert_eq!(
            resolve_events_dir(&env(None, tmp2.path())),
            EventDestination::Off
        );
    }

    #[test]
    fn keel_events_stderr_selects_stderr_and_never_touches_the_filesystem() {
        // #94 (partial): KEEL_EVENTS=stderr must win regardless of whether
        // `.keel` exists, and must never resolve to a directory — a
        // regression here would silently start writing (or looking for) a
        // file again.
        let tmp = tempfile::tempdir().expect("tempdir");
        for spelling in ["stderr", "STDERR", "StdErr", " stderr "] {
            assert_eq!(
                resolve_events_dir(&env(Some(spelling), tmp.path())),
                EventDestination::Stderr,
                "{spelling:?}"
            );
        }
        // Even with a real `.keel` dir present, stderr still wins (it is not
        // shadowed by the unset-with-.keel-present branch).
        std::fs::create_dir(tmp.path().join(".keel")).expect("mk .keel");
        assert_eq!(
            resolve_events_dir(&env(Some("stderr"), tmp.path())),
            EventDestination::Stderr
        );
        assert!(
            !tmp.path().join(".keel").join(EVENTS_SUBDIR).exists(),
            "resolving to Stderr must not create .keel/events"
        );
    }

    #[test]
    fn open_stderr_is_not_file_backed_and_creates_no_directory() {
        // Regression guard for the destination-change: `open_stderr` must
        // never carry a `path()` (it is not file-backed) and must never
        // touch the filesystem, unlike `EventSink::open`. This does write one
        // `run_start` line to the real process stderr — that IS the feature.
        let cwd = std::env::current_dir().expect("cwd");
        let sink = EventSink::open_stderr().expect("stderr sink must start");
        assert_eq!(sink.path(), None, "stderr sink is not file-backed");
        assert!(
            !cwd.join(".keel").join(EVENTS_SUBDIR).exists(),
            "open_stderr must not create .keel/events"
        );
        drop(sink);
    }

    #[test]
    fn trace_ref_round_trips_and_rejects_junk() {
        let r = TraceRef {
            run: "019a2b3c4d5-7f2e".to_owned(),
            seq: 12,
        };
        assert_eq!(r.to_string(), "019a2b3c4d5-7f2e#12");
        assert_eq!(r.file_name(), "019a2b3c4d5-7f2e.ndjson");
        assert_eq!("019a2b3c4d5-7f2e#12".parse::<TraceRef>(), Ok(r));
        // A '#' inside the run id resolves to the LAST separator.
        assert_eq!(
            "a#b#3".parse::<TraceRef>(),
            Ok(TraceRef {
                run: "a#b".to_owned(),
                seq: 3
            })
        );
        for bad in ["", "norun", "#7", "run#", "run#x", "run#-1"] {
            assert_eq!(bad.parse::<TraceRef>(), Err(ParseTraceRefError), "{bad:?}");
        }
    }

    #[test]
    fn sink_writes_header_then_events_in_seq_order_and_drop_flushes() {
        let buf = SharedBuf::default();
        let sink =
            EventSink::to_writer(Box::new(buf.clone()), "run-test").expect("sink must start");
        assert_eq!(sink.run_id(), "run-test");
        assert_eq!(sink.path(), None);
        let seq = sink.emit(
            5,
            EventKind::CallStart {
                call: "t-000001".to_owned(),
                target: "api.example.com".to_owned(),
                op: "GET api.example.com".to_owned(),
            },
        );
        assert_eq!(seq, 1, "run_start header owns seq 0");
        drop(sink); // joins the writer: everything queued is on disk after this

        let lines: Vec<Event> = buf
            .contents()
            .lines()
            .map(|l| serde_json::from_str(l).expect("every line parses"))
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines[0],
            Event {
                v: 1,
                seq: 0,
                ms: 0,
                kind: EventKind::RunStart {
                    run: "run-test".to_owned(),
                    wall_ms: None,
                    pid: None,
                },
            }
        );
        assert_eq!(lines[1].seq, 1);
        assert_eq!(lines[1].ms, 5);
        // Pin the exact wire shape of the header: field order is part of the
        // format a later `keel tail` golden-tests against.
        assert_eq!(
            buf.contents().lines().next().expect("header line"),
            r#"{"v":1,"seq":0,"ms":0,"event":"run_start","run":"run-test"}"#
        );
    }

    #[test]
    fn flush_makes_the_feed_current_without_dropping_the_sink() {
        let buf = SharedBuf::default();
        let sink =
            EventSink::to_writer(Box::new(buf.clone()), "run-flush").expect("sink must start");
        sink.emit(
            1,
            EventKind::BreakerReject {
                call: "t-000001".to_owned(),
                target: "api.example.com".to_owned(),
            },
        );
        sink.flush();
        let contents = buf.contents();
        assert!(
            contents.contains(r#""event":"breaker_reject""#),
            "flushed feed must contain the event: {contents}"
        );
    }

    #[test]
    fn file_backed_sink_writes_run_file_with_wall_header() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join(".keel").join(EVENTS_SUBDIR);
        let sink = EventSink::open(&dir).expect("open sink");
        let run = sink.run_id().to_owned();
        let path = sink.path().expect("file-backed").to_owned();
        assert_eq!(path, dir.join(format!("{run}.ndjson")));
        drop(sink);

        let feed = std::fs::read_to_string(&path).expect("feed readable");
        let header: Event =
            serde_json::from_str(feed.lines().next().expect("header")).expect("header parses");
        match header.kind {
            EventKind::RunStart {
                run: r,
                wall_ms,
                pid,
            } => {
                assert_eq!(r, run);
                assert!(wall_ms.is_some(), "production header anchors wall time");
                assert_eq!(pid, Some(std::process::id()));
            }
            other => panic!("first line must be run_start, got {other:?}"),
        }
    }

    #[test]
    fn events_env_capture_reads_process_state() {
        let env = EventsEnv::capture();
        assert_ne!(env.base_dir, PathBuf::new(), "cwd captured");
    }
}
