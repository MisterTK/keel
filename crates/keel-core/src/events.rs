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
//! environment ([`EventSink::from_env`], testable as [`resolve_events_dir`]):
//!
//! - `KEEL_EVENTS` set to `0` / `false` / `off` / empty — force **off**.
//! - `KEEL_EVENTS` set to anything else (`1`, `true`, …) — force **on**
//!   (creates `./.keel/events/` on demand).
//! - unset — **on** exactly when `./.keel` already exists (a Keel-initialized
//!   project directory), **off** otherwise.
//!
//! Off is a zero-cost no-op: the engine holds no sink and every emit site is
//! one `Option` discriminant check (the overhead bench's `a_empty` /
//! `b_cache_miss` / `c_cache_hit` cases run this path; `d_events` measures
//! the on path). A sink that cannot open (unwritable directory) degrades to a
//! `warn!` and off — observability never fails the wrapped call.
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
    /// The directory whose `.keel/` marks a Keel-initialized project (the
    /// process working directory in production).
    pub base_dir: PathBuf,
}

impl EventsEnv {
    /// Snapshot the real process environment.
    #[must_use]
    pub fn capture() -> Self {
        Self {
            keel_events: std::env::var("KEEL_EVENTS").ok(),
            base_dir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        }
    }
}

/// Where events should be written, or `None` when the sink is off. See the
/// module docs for the decision table this implements.
#[must_use]
pub fn resolve_events_dir(env: &EventsEnv) -> Option<PathBuf> {
    let keel_dir = env.base_dir.join(".keel");
    match env.keel_events.as_deref().map(str::trim) {
        Some(v)
            if v.is_empty()
                || v.eq_ignore_ascii_case("0")
                || v.eq_ignore_ascii_case("false")
                || v.eq_ignore_ascii_case("off") =>
        {
            None
        }
        Some(_) => Some(keel_dir.join(EVENTS_SUBDIR)),
        None => keel_dir.is_dir().then(|| keel_dir.join(EVENTS_SUBDIR)),
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
    /// open the per-run file. `None` when off — or when the sink cannot open,
    /// which degrades to a `warn!` (observability never fails the call).
    #[must_use]
    pub fn from_env() -> Option<Self> {
        let dir = resolve_events_dir(&EventsEnv::capture())?;
        match Self::open(&dir) {
            Ok(sink) => Some(sink),
            Err(error) => {
                warn!(dir = %dir.display(), error = %error, "event sink unavailable; live events disabled");
                None
            }
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
fn write_events(rx: &Receiver<Msg>, mut out: Box<dyn Write + Send>) {
    let mut dirty = false;
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
                if serde_json::to_writer(&mut out, &event).is_ok() && out.write_all(b"\n").is_ok() {
                    dirty = true;
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
        EVENTS_SUBDIR, Event, EventKind, EventSink, EventsEnv, ParseTraceRefError, TraceRef,
        resolve_events_dir,
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
    fn activation_decision_table() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let bare = tmp.path(); // no .keel yet

        // Unset + no .keel dir: off.
        assert_eq!(resolve_events_dir(&env(None, bare)), None);
        // Explicitly off, in every accepted spelling, beats everything.
        for off in ["0", "false", "off", "FALSE", "Off", "", "  "] {
            assert_eq!(resolve_events_dir(&env(Some(off), bare)), None, "{off:?}");
        }
        // Any other set value forces on, .keel dir or not.
        let expected = bare.join(".keel").join(EVENTS_SUBDIR);
        for on in ["1", "true", "on", "yes"] {
            assert_eq!(
                resolve_events_dir(&env(Some(on), bare)),
                Some(expected.clone()),
                "{on:?}"
            );
        }
        // Unset + an existing .keel dir: on (the keel-initialized project case).
        std::fs::create_dir(bare.join(".keel")).expect("mk .keel");
        assert_eq!(resolve_events_dir(&env(None, bare)), Some(expected));
        // A .keel FILE is not a project marker.
        let tmp2 = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp2.path().join(".keel"), b"not a dir").expect("write file");
        assert_eq!(resolve_events_dir(&env(None, tmp2.path())), None);
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
