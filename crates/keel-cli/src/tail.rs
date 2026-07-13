//! `keel tail` — the live view of what Keel is doing for a running program:
//! attempts, backoff waits, breaker open/half-open/close, rate-limit queueing,
//! cache hits (dx-spec §6 — "the conversion moment").
//!
//! # Where the events come from
//!
//! The engine's event sink (`keel-core/src/events.rs`) appends one NDJSON
//! line per event to `.keel/events/<run>.ndjson`. `keel tail` only *reads*
//! those files — no daemon, no IPC (dx invariant 3). The line format is
//! versioned (`"v": 1`) but non-contract; this module parses lines as plain
//! JSON values so an unknown future event degrades to a generic line instead
//! of a parse failure.
//!
//! # Modes
//!
//! - default: follow the newest run file, polling for appended lines and for
//!   a newer run superseding it (a new `keel run` mid-tail rotates the view;
//!   the new run's `run_start` header announces the switch). Runs until
//!   interrupted.
//! - `--run <id>`: pin one run; rotation is disabled.
//! - `--no-follow`: render what is already recorded and exit — the
//!   deterministic snapshot mode (golden-testable byte-for-byte).
//! - `--json`: raw NDJSON passthrough with sorted keys (serde_json maps are
//!   `BTreeMap`s), one event per line — an agent can stream-parse it.
//!
//! # Determinism and color
//!
//! The human renderer derives everything from the event line (`ms` is
//! engine-elapsed, never wall time), so `--no-follow` output is byte-stable
//! for a fixed feed. ANSI color is plain SGR, applied only when stdout is a
//! terminal and `NO_COLOR` is unset ([`color_enabled`]); the `color` flag is
//! explicit in [`TailOptions`] so tests pin both looks.

use std::fs::File;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

use crate::render::to_json;
use crate::{EXIT_FAILURE, Rendered};

/// File extension of a run's event feed (mirrors the sink; non-contract).
const EVENTS_EXT: &str = "ndjson";

/// How often the production follow loop polls for new bytes / newer runs.
const POLL_INTERVAL: Duration = Duration::from_millis(150);

/// What `keel tail` was asked to do. `follow` is the default CLI behavior;
/// `--no-follow` flips it off for deterministic snapshots.
#[derive(Debug, Clone)]
pub struct TailOptions {
    /// Apply ANSI color to the human view (resolved by [`color_enabled`]).
    pub color: bool,
    /// Keep polling for new events (and newer runs) until stopped.
    pub follow: bool,
    /// Emit raw NDJSON (sorted keys) instead of the human view.
    pub json: bool,
    /// Pin a specific run id instead of following the newest run.
    pub run: Option<String>,
}

/// Drives the follow loop's waiting, injected so tests never sleep for real:
/// each call separates two polls and its return value decides continuation.
pub trait Ticker {
    /// Wait one poll interval. Return `false` to stop following — the
    /// production ticker never does; `keel tail` runs until interrupted.
    fn tick(&mut self) -> bool;
}

/// Production ticker: a plain sleep between cheap re-reads (no file-watcher
/// dependency; the feed is a local append-only file).
#[derive(Debug)]
pub struct SleepTicker {
    interval: Duration,
}

impl Default for SleepTicker {
    fn default() -> Self {
        Self {
            interval: POLL_INTERVAL,
        }
    }
}

impl Ticker for SleepTicker {
    fn tick(&mut self) -> bool {
        std::thread::sleep(self.interval);
        true
    }
}

/// Whether the human view should use ANSI color: stdout is a terminal and
/// `NO_COLOR` is unset or empty (<https://no-color.org>).
#[must_use]
pub fn color_enabled() -> bool {
    let no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
    !no_color && io::stdout().is_terminal()
}

/// Run `keel tail` for `project`, writing rendered events to `out`. Errors
/// (no `.keel/`, no runs in snapshot mode, an unknown `--run`) come back as a
/// [`Rendered`] guidance report for the caller to emit; success streams to
/// `out` and returns once following stops ([`Ticker::tick`] returning false,
/// or immediately under `--no-follow`).
pub fn run(
    project: &Path,
    opts: &TailOptions,
    out: &mut dyn Write,
    ticker: &mut dyn Ticker,
) -> Result<(), Rendered> {
    let keel = project.join(".keel");
    if !keel.is_dir() {
        return Err(guidance(
            "nothing to tail \u{2014} this directory has no .keel/.",
            "Keel projects keep live event feeds in .keel/events/<run>.ndjson; \
             the sink turns on when .keel/ exists (or KEEL_EVENTS=1 is set).",
            "run `keel init` here, start your program with `keel run <script>`, \
             then `keel tail`.",
        ));
    }
    let dir = keel.join("events");

    let mut cursor: Option<Cursor> = None;
    let mut announced_wait = false;
    loop {
        if cursor.is_none() {
            match select_run(&dir, opts.run.as_deref())? {
                Some(path) => {
                    cursor = Some(Cursor::open(&path).map_err(|e| unreadable(&path, &e))?)
                }
                None if !opts.follow => return Err(no_runs()),
                None => {
                    if !opts.json && !announced_wait {
                        let _ = writeln!(
                            out,
                            "waiting for a run\u{2026} (.keel/events/ is empty; start a program under `keel run`)"
                        );
                        announced_wait = true;
                    }
                }
            }
        }
        if let Some(c) = cursor.as_mut()
            && c.drain(opts, out).is_err()
        {
            // The reader went away (e.g. `keel tail | head`); stop cleanly.
            return Ok(());
        }
        // Run rotation: a strictly newer run supersedes the one being
        // followed — its run_start header announces the switch. Never when
        // --run pinned the choice.
        if opts.follow
            && opts.run.is_none()
            && let Some(current) = cursor.as_ref()
            && let Some((stem, path)) = newest_run(&dir)
            && stem > current.stem
        {
            cursor = Some(Cursor::open(&path).map_err(|e| unreadable(&path, &e))?);
            continue; // drain the new run before the next tick
        }
        let _ = out.flush();
        if !opts.follow || !ticker.tick() {
            break;
        }
    }
    let _ = out.flush();
    Ok(())
}

/// Pick the event file to read: the pinned run when `--run` was given (an
/// unknown id is a guidance error listing what exists), otherwise the newest
/// run by name — run ids are zero-padded hex epoch-milliseconds, so the
/// lexically greatest stem is the latest run. `None` means no runs yet.
fn select_run(dir: &Path, pinned: Option<&str>) -> Result<Option<PathBuf>, Rendered> {
    let runs = runs_in(dir);
    match pinned {
        Some(run) if runs.iter().any(|r| r == run) => {
            Ok(Some(dir.join(format!("{run}.{EVENTS_EXT}"))))
        }
        Some(run) => Err(unknown_run(run, &runs)),
        None => Ok(runs.last().map(|r| dir.join(format!("{r}.{EVENTS_EXT}")))),
    }
}

/// The run ids recorded under `dir` (stems of `*.ndjson`), sorted ascending.
fn runs_in(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut runs: Vec<String> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some(EVENTS_EXT) {
                return None;
            }
            path.file_stem().and_then(|s| s.to_str()).map(str::to_owned)
        })
        .collect();
    runs.sort_unstable();
    runs
}

/// The newest run's `(stem, path)` under `dir`, if any.
fn newest_run(dir: &Path) -> Option<(String, PathBuf)> {
    let stem = runs_in(dir).pop()?;
    let path = dir.join(format!("{stem}.{EVENTS_EXT}"));
    Some((stem, path))
}

/// A position in one run's feed. The open file handle keeps its own read
/// offset (appends show up on the next read); `carry` buffers a trailing
/// partial line until its newline arrives, so a mid-write line is never
/// rendered half-formed.
struct Cursor {
    stem: String,
    file: File,
    carry: Vec<u8>,
}

impl Cursor {
    fn open(path: &Path) -> io::Result<Self> {
        Ok(Self {
            stem: path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_owned(),
            file: File::open(path)?,
            carry: Vec::new(),
        })
    }

    /// Read newly appended bytes and render every *complete* line. Read
    /// errors are treated as "no new data" (transient; the next poll
    /// retries); write errors propagate — the output side is gone.
    fn drain(&mut self, opts: &TailOptions, out: &mut dyn Write) -> io::Result<()> {
        let mut buf = Vec::new();
        if self.file.read_to_end(&mut buf).is_ok() {
            self.carry.extend_from_slice(&buf);
        }
        while let Some(pos) = self.carry.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.carry.drain(..=pos).collect();
            let text = String::from_utf8_lossy(&line[..pos]);
            render_line(text.trim_end_matches('\r'), opts, out)?;
        }
        Ok(())
    }
}

/// Render one feed line: JSON mode re-serializes the parsed value (compact,
/// keys sorted by serde_json's `BTreeMap`); human mode formats it via
/// [`human_line`]. Lines that are not JSON objects are skipped — the feed is
/// best-effort observability, never a hard failure.
fn render_line(line: &str, opts: &TailOptions, out: &mut dyn Write) -> io::Result<()> {
    if line.trim().is_empty() {
        return Ok(());
    }
    let Ok(value) = serde_json::from_str::<Value>(line) else {
        return Ok(());
    };
    if !value.is_object() {
        return Ok(());
    }
    if opts.json {
        return writeln!(out, "{value}");
    }
    if let Some(text) = human_line(&value, opts.color) {
        writeln!(out, "{text}")?;
    }
    Ok(())
}

/// ANSI tones the human view uses. `Plain` renders no codes at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tone {
    Cyan,
    Dim,
    Green,
    Plain,
    Red,
    Yellow,
}

/// Wrap `text` in the tone's SGR codes when color is on. Padding must be
/// applied *before* painting — the escape codes have zero display width.
fn paint(text: &str, tone: Tone, color: bool) -> String {
    let code = match tone {
        _ if !color => return text.to_owned(),
        Tone::Plain => return text.to_owned(),
        Tone::Cyan => "36",
        Tone::Dim => "2",
        Tone::Green => "32",
        Tone::Red => "31",
        Tone::Yellow => "33",
    };
    format!("\u{1b}[{code}m{text}\u{1b}[0m")
}

/// Engine-elapsed `ms` as a fixed-width clock, `mm:ss.mmm` (grows past 99
/// minutes naturally).
fn fmt_clock(ms: u64) -> String {
    let minutes = ms / 60_000;
    let seconds = (ms % 60_000) / 1000;
    let millis = ms % 1000;
    format!("{minutes:02}:{seconds:02}.{millis:03}")
}

/// A wait/cooldown duration for humans: `450ms` below a second, else seconds
/// with tenths (`1.5s`, `30s`).
fn fmt_wait(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms % 1000 == 0 {
        format!("{}s", ms / 1000)
    } else {
        format!("{}.{}s", ms / 1000, (ms % 1000) / 100)
    }
}

/// "1 attempt" / "3 attempts".
fn fmt_attempts(n: u64) -> String {
    if n == 1 {
        "1 attempt".to_owned()
    } else {
        format!("{n} attempts")
    }
}

/// Format one parsed event for humans:
/// `mm:ss.mmm  <call>  <target>  <verb>  <detail>`, with the verb tone-coded
/// (green ok / yellow degradation / red failure). Unknown event kinds render
/// generically (dim verb, no detail) — the vocabulary may grow. Returns
/// `None` only when the line has no `ms`/`event` envelope at all.
#[expect(
    clippy::too_many_lines,
    reason = "one match arm per event kind; splitting it would hide the vocabulary"
)]
fn human_line(event: &Value, color: bool) -> Option<String> {
    let ms = event.get("ms").and_then(Value::as_u64)?;
    let kind = event.get("event").and_then(Value::as_str)?;
    let time = paint(&fmt_clock(ms), Tone::Dim, color);

    if kind == "run_start" {
        let run = event.get("run").and_then(Value::as_str).unwrap_or("?");
        let pid = event
            .get("pid")
            .and_then(Value::as_u64)
            .map(|p| format!(" (pid {p})"))
            .unwrap_or_default();
        return Some(format!(
            "{time}  {} {run}{pid}",
            paint("run", Tone::Dim, color)
        ));
    }

    let call = event.get("call").and_then(Value::as_str).unwrap_or("-");
    let target = event.get("target").and_then(Value::as_str).unwrap_or("-");
    let attempt = event.get("attempt").and_then(Value::as_u64).unwrap_or(0);
    let wait_ms = event.get("wait_ms").and_then(Value::as_u64).unwrap_or(0);
    let scope = event.get("scope").and_then(Value::as_str).unwrap_or("?");

    let (verb, tone, detail) = match kind {
        "call_start" => (
            "call",
            Tone::Cyan,
            event
                .get("op")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
        ),
        "cache_hit" => ("cache", Tone::Green, format!("hit ({scope})")),
        "cache_miss" => ("cache", Tone::Dim, format!("miss ({scope})")),
        "throttle" => (
            "rate",
            Tone::Yellow,
            format!("queued {}", fmt_wait(wait_ms)),
        ),
        "breaker_reject" => (
            "breaker",
            Tone::Red,
            "rejected \u{2014} open, failing fast".to_owned(),
        ),
        "breaker_half_open" => (
            "breaker",
            Tone::Yellow,
            "half-open \u{2014} probing".to_owned(),
        ),
        "breaker_open" => {
            let cooldown = event
                .get("cooldown_ms")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            (
                "breaker",
                Tone::Red,
                format!("opened (cooldown {})", fmt_wait(cooldown)),
            )
        }
        "breaker_close" => ("breaker", Tone::Green, "closed".to_owned()),
        "attempt_start" => ("attempt", Tone::Plain, format!("#{attempt}")),
        "attempt_error" => {
            let class = event.get("class").and_then(Value::as_str).unwrap_or("?");
            let status = event
                .get("http_status")
                .and_then(Value::as_u64)
                .map(|s| format!(" {s}"))
                .unwrap_or_default();
            ("fail", Tone::Yellow, format!("#{attempt} {class}{status}"))
        }
        "backoff" => (
            "backoff",
            Tone::Yellow,
            format!("{} \u{2192} #{}", fmt_wait(wait_ms), attempt + 1),
        ),
        "call_end" => {
            let attempts = event.get("attempts").and_then(Value::as_u64).unwrap_or(0);
            if event.get("result").and_then(Value::as_str) == Some("ok") {
                ("ok", Tone::Green, fmt_attempts(attempts))
            } else {
                let code = event.get("code").and_then(Value::as_str).unwrap_or("error");
                (
                    "error",
                    Tone::Red,
                    format!("{code} after {}", fmt_attempts(attempts)),
                )
            }
        }
        other => (other, Tone::Dim, String::new()),
    };

    let verb_padded = format!("{verb:<8}");
    let line = format!(
        "{time}  {call:<9} {target:<24} {} {detail}",
        paint(&verb_padded, tone, color)
    );
    Some(line.trim_end().to_owned())
}

/// A what/why/next guidance report (exit 1, stderr) — the tail equivalents of
/// `keel flows`' soft errors, with the remedy spelled out.
fn guidance(what: &str, why: &str, next: &str) -> Rendered {
    #[derive(Serialize)]
    struct Guidance<'a> {
        error: &'a str,
        next: &'a str,
        why: &'a str,
    }
    Rendered {
        human: format!("keel \u{25b8} {what}\n  why:  {why}\n  next: {next}"),
        json: to_json(&Guidance {
            error: what,
            next,
            why,
        }),
        exit: EXIT_FAILURE,
        to_stderr: true,
    }
}

/// `--no-follow` with nothing recorded yet.
fn no_runs() -> Rendered {
    guidance(
        "no runs recorded yet \u{2014} .keel/events/ is empty.",
        "each program run under Keel appends its live events to \
         .keel/events/<run>.ndjson (on by default in a Keel project; \
         KEEL_EVENTS=1 forces it).",
        "start your program with `keel run <script>`, then re-run `keel tail` \
         \u{2014} without --no-follow it waits for the run to appear.",
    )
}

/// `--run <id>` named a run that is not on disk.
fn unknown_run(run: &str, available: &[String]) -> Rendered {
    let why = if available.is_empty() {
        "no runs are recorded under .keel/events/ yet.".to_owned()
    } else {
        let newest_first: Vec<&str> = available.iter().rev().take(5).map(String::as_str).collect();
        format!(
            "available runs (newest first): {}.",
            newest_first.join(", ")
        )
    };
    guidance(
        &format!("run {run:?} not found under .keel/events/."),
        &why,
        "`keel tail --run <id>` with a recorded id, or plain `keel tail` for \
         the newest run.",
    )
}

/// An event file that exists but cannot be opened/read.
fn unreadable(path: &Path, error: &io::Error) -> Rendered {
    guidance(
        &format!("cannot read {}.", path.display()),
        &format!("{error}."),
        "check the file's permissions, or remove it and re-run the program.",
    )
}

#[cfg(test)]
mod tests {
    use super::{
        Cursor, TailOptions, Ticker, fmt_clock, fmt_wait, human_line, paint, run, runs_in,
        select_run,
    };
    use serde_json::{Value, json};
    use std::io::Write as _;
    use std::path::{Path, PathBuf};

    fn opts(follow: bool, json: bool) -> TailOptions {
        TailOptions {
            color: false,
            follow,
            json,
            run: None,
        }
    }

    /// A ticker driven by a closure over the tick index — tests script file
    /// mutations between polls and bound the loop, with zero real sleeps.
    struct ScriptTicker<F: FnMut(usize) -> bool> {
        n: usize,
        f: F,
    }

    impl<F: FnMut(usize) -> bool> ScriptTicker<F> {
        fn new(f: F) -> Self {
            Self { n: 0, f }
        }
    }

    impl<F: FnMut(usize) -> bool> Ticker for ScriptTicker<F> {
        fn tick(&mut self) -> bool {
            let go = (self.f)(self.n);
            self.n += 1;
            go
        }
    }

    /// A never-ticking ticker for `--no-follow` calls.
    fn no_tick() -> ScriptTicker<impl FnMut(usize) -> bool> {
        ScriptTicker::new(|_| false)
    }

    fn project_with_events(files: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::TempDir::new().unwrap();
        let events = dir.path().join(".keel").join("events");
        std::fs::create_dir_all(&events).unwrap();
        for (run, body) in files {
            std::fs::write(events.join(format!("{run}.ndjson")), body).unwrap();
        }
        let project = dir.path().to_path_buf();
        (dir, project)
    }

    fn tail_to_string(project: &Path, o: &TailOptions, ticker: &mut dyn Ticker) -> String {
        let mut out = Vec::new();
        run(project, o, &mut out, ticker).expect("tail succeeds");
        String::from_utf8(out).unwrap()
    }

    fn line(event: &Value) -> Option<String> {
        human_line(event, false)
    }

    // ---- renderer -------------------------------------------------------

    #[test]
    fn renders_every_event_kind() {
        let base = json!({"call": "t-000001", "target": "api.example.com"});
        let ev = |ms: u64, tag: &str, extra: Value| {
            let mut v = base.clone();
            let obj = v.as_object_mut().unwrap();
            obj.insert("ms".into(), json!(ms));
            obj.insert("event".into(), json!(tag));
            for (k, val) in extra.as_object().unwrap() {
                obj.insert(k.clone(), val.clone());
            }
            v
        };

        assert_eq!(
            line(&json!({"ms": 0, "event": "run_start", "run": "r-1", "pid": 42})).unwrap(),
            "00:00.000  run r-1 (pid 42)"
        );
        assert_eq!(
            line(&json!({"ms": 0, "event": "run_start", "run": "r-1"})).unwrap(),
            "00:00.000  run r-1"
        );
        assert_eq!(
            line(&ev(5, "call_start", json!({"op": "GET api.example.com"}))).unwrap(),
            "00:00.005  t-000001  api.example.com          call     GET api.example.com"
        );
        assert_eq!(
            line(&ev(5, "cache_hit", json!({"scope": "memory"}))).unwrap(),
            "00:00.005  t-000001  api.example.com          cache    hit (memory)"
        );
        assert_eq!(
            line(&ev(5, "cache_miss", json!({"scope": "persistent"}))).unwrap(),
            "00:00.005  t-000001  api.example.com          cache    miss (persistent)"
        );
        assert_eq!(
            line(&ev(5, "throttle", json!({"wait_ms": 150}))).unwrap(),
            "00:00.005  t-000001  api.example.com          rate     queued 150ms"
        );
        assert_eq!(
            line(&ev(5, "breaker_reject", json!({}))).unwrap(),
            "00:00.005  t-000001  api.example.com          breaker  rejected \u{2014} open, failing fast"
        );
        assert_eq!(
            line(&ev(5, "breaker_half_open", json!({}))).unwrap(),
            "00:00.005  t-000001  api.example.com          breaker  half-open \u{2014} probing"
        );
        assert_eq!(
            line(&ev(5, "breaker_open", json!({"cooldown_ms": 30000}))).unwrap(),
            "00:00.005  t-000001  api.example.com          breaker  opened (cooldown 30s)"
        );
        assert_eq!(
            line(&ev(5, "breaker_close", json!({}))).unwrap(),
            "00:00.005  t-000001  api.example.com          breaker  closed"
        );
        assert_eq!(
            line(&ev(5, "attempt_start", json!({"attempt": 2}))).unwrap(),
            "00:00.005  t-000001  api.example.com          attempt  #2"
        );
        assert_eq!(
            line(&ev(
                5,
                "attempt_error",
                json!({"attempt": 1, "class": "http", "http_status": 503})
            ))
            .unwrap(),
            "00:00.005  t-000001  api.example.com          fail     #1 http 503"
        );
        assert_eq!(
            line(&ev(
                5,
                "attempt_error",
                json!({"attempt": 2, "class": "timeout"})
            ))
            .unwrap(),
            "00:00.005  t-000001  api.example.com          fail     #2 timeout"
        );
        assert_eq!(
            line(&ev(5, "backoff", json!({"attempt": 1, "wait_ms": 200}))).unwrap(),
            "00:00.005  t-000001  api.example.com          backoff  200ms \u{2192} #2"
        );
        assert_eq!(
            line(&ev(5, "call_end", json!({"result": "ok", "attempts": 1}))).unwrap(),
            "00:00.005  t-000001  api.example.com          ok       1 attempt"
        );
        assert_eq!(
            line(&ev(
                5,
                "call_end",
                json!({"result": "error", "code": "KEEL-E010", "attempts": 3})
            ))
            .unwrap(),
            "00:00.005  t-000001  api.example.com          error    KEEL-E010 after 3 attempts"
        );
        // An event kind this build has never heard of still gets a line.
        assert_eq!(
            line(&ev(5, "budget_exceeded", json!({}))).unwrap(),
            "00:00.005  t-000001  api.example.com          budget_exceeded"
        );
        // No envelope at all → nothing to render.
        assert_eq!(line(&json!({"note": "not an event"})), None);
    }

    #[test]
    fn color_paints_the_verb_without_breaking_alignment() {
        let ev = json!({
            "ms": 5, "event": "call_start", "call": "t-000001",
            "target": "api.example.com", "op": "GET api.example.com"
        });
        let colored = human_line(&ev, true).unwrap();
        // Verb padded first, then wrapped — codes sit outside the 8 columns.
        assert!(colored.contains("\u{1b}[36mcall    \u{1b}[0m"));
        // Time column is dimmed.
        assert!(colored.starts_with("\u{1b}[2m00:00.005\u{1b}[0m"));
        // And the plain tone paints nothing.
        assert_eq!(paint("attempt ", super::Tone::Plain, true), "attempt ");
    }

    #[test]
    fn clock_and_wait_formats() {
        assert_eq!(fmt_clock(0), "00:00.000");
        assert_eq!(fmt_clock(31_080), "00:31.080");
        assert_eq!(fmt_clock(61_005), "01:01.005");
        assert_eq!(fmt_clock(6_000_000), "100:00.000");
        assert_eq!(fmt_wait(450), "450ms");
        assert_eq!(fmt_wait(1000), "1s");
        assert_eq!(fmt_wait(1550), "1.5s");
        assert_eq!(fmt_wait(30_000), "30s");
    }

    // ---- selection and errors -------------------------------------------

    #[test]
    fn newest_run_wins_by_name_sort_and_pin_overrides() {
        let (_d, project) = project_with_events(&[
            (
                "0000000f00d-0001",
                "{\"v\":1,\"seq\":0,\"ms\":0,\"event\":\"run_start\",\"run\":\"0000000f00d-0001\"}\n",
            ),
            (
                "0000000f00e-0002",
                "{\"v\":1,\"seq\":0,\"ms\":0,\"event\":\"run_start\",\"run\":\"0000000f00e-0002\"}\n",
            ),
        ]);
        let dir = project.join(".keel").join("events");
        assert_eq!(runs_in(&dir), vec!["0000000f00d-0001", "0000000f00e-0002"]);

        let newest = tail_to_string(&project, &opts(false, false), &mut no_tick());
        assert!(newest.contains("run 0000000f00e-0002"));
        assert!(!newest.contains("run 0000000f00d-0001"));

        let mut pinned = opts(false, false);
        pinned.run = Some("0000000f00d-0001".to_owned());
        let old = tail_to_string(&project, &pinned, &mut no_tick());
        assert!(old.contains("run 0000000f00d-0001"));
    }

    #[test]
    fn unknown_pinned_run_lists_whats_available() {
        let (_d, project) = project_with_events(&[("0000000f00d-0001", "")]);
        let dir = project.join(".keel").join("events");
        let err = select_run(&dir, Some("zzz")).unwrap_err();
        assert_eq!(err.exit, crate::EXIT_FAILURE);
        assert!(err.to_stderr);
        assert!(err.human.contains("run \"zzz\" not found"));
        assert!(err.human.contains("0000000f00d-0001"));
        assert_eq!(
            err.json["error"],
            "run \"zzz\" not found under .keel/events/."
        );
    }

    #[test]
    fn missing_keel_dir_explains_what_why_next() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut out = Vec::new();
        let err = run(dir.path(), &opts(false, false), &mut out, &mut no_tick()).unwrap_err();
        assert_eq!(err.exit, crate::EXIT_FAILURE);
        assert!(err.to_stderr);
        assert!(err.human.contains("nothing to tail"));
        assert!(err.human.contains("why:"));
        assert!(err.human.contains("next:"));
        assert!(err.json["next"].as_str().unwrap().contains("keel init"));
    }

    #[test]
    fn empty_events_dir_without_follow_explains() {
        let (_d, project) = project_with_events(&[]);
        let mut out = Vec::new();
        let err = run(&project, &opts(false, false), &mut out, &mut no_tick()).unwrap_err();
        assert!(err.human.contains("no runs recorded yet"));
    }

    // ---- following ------------------------------------------------------

    #[test]
    fn follow_picks_up_lines_appended_mid_run() {
        let (_d, project) = project_with_events(&[(
            "run-a",
            "{\"v\":1,\"seq\":0,\"ms\":0,\"event\":\"run_start\",\"run\":\"run-a\"}\n",
        )]);
        let feed = project.join(".keel").join("events").join("run-a.ndjson");
        let appended = "{\"v\":1,\"seq\":1,\"ms\":7,\"event\":\"call_start\",\"call\":\"t-000001\",\"target\":\"api.example.com\",\"op\":\"GET api.example.com\"}";
        let mut ticker = ScriptTicker::new(move |n| {
            if n == 0 {
                let mut f = std::fs::OpenOptions::new()
                    .append(true)
                    .open(&feed)
                    .unwrap();
                writeln!(f, "{appended}").unwrap();
                true
            } else {
                false
            }
        });
        let text = tail_to_string(&project, &opts(true, false), &mut ticker);
        assert!(text.contains("run run-a"));
        assert!(text.contains("call     GET api.example.com"));
    }

    #[test]
    fn follow_buffers_a_partial_line_until_its_newline_arrives() {
        let full = "{\"v\":1,\"seq\":1,\"ms\":7,\"event\":\"call_start\",\"call\":\"t-000001\",\"target\":\"api.example.com\",\"op\":\"GET api.example.com\"}";
        let (head, rest) = full.split_at(40);
        let (_d, project) = project_with_events(&[("run-a", head)]);
        let feed = project.join(".keel").join("events").join("run-a.ndjson");

        // Snapshot mode sees no complete line yet.
        let rest_owned = rest.to_owned();
        let empty = tail_to_string(&project, &opts(false, true), &mut no_tick());
        assert_eq!(empty, "");

        let mut ticker = ScriptTicker::new(move |n| {
            if n == 0 {
                let mut f = std::fs::OpenOptions::new()
                    .append(true)
                    .open(&feed)
                    .unwrap();
                writeln!(f, "{rest_owned}").unwrap();
                true
            } else {
                false
            }
        });
        let text = tail_to_string(&project, &opts(true, true), &mut ticker);
        // The reassembled line comes out exactly once, keys sorted.
        assert_eq!(text.lines().count(), 1);
        assert!(text.starts_with("{\"call\":\"t-000001\""));
    }

    #[test]
    fn follow_rotates_to_a_newer_run() {
        let (_d, project) = project_with_events(&[(
            "0000000f00d-0001",
            "{\"v\":1,\"seq\":0,\"ms\":0,\"event\":\"run_start\",\"run\":\"0000000f00d-0001\"}\n",
        )]);
        let events = project.join(".keel").join("events");
        let mut ticker = ScriptTicker::new(move |n| {
            if n == 0 {
                std::fs::write(
                    events.join("0000000f00e-0002.ndjson"),
                    "{\"v\":1,\"seq\":0,\"ms\":0,\"event\":\"run_start\",\"run\":\"0000000f00e-0002\"}\n",
                )
                .unwrap();
                true
            } else {
                false
            }
        });
        let text = tail_to_string(&project, &opts(true, false), &mut ticker);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "old header then new header: {text}");
        assert!(lines[0].contains("run 0000000f00d-0001"));
        assert!(lines[1].contains("run 0000000f00e-0002"));
    }

    #[test]
    fn follow_waits_for_the_first_run_and_says_so() {
        let (_d, project) = project_with_events(&[]);
        let events = project.join(".keel").join("events");
        let mut ticker = ScriptTicker::new(move |n| {
            if n == 0 {
                std::fs::write(
                    events.join("run-late.ndjson"),
                    "{\"v\":1,\"seq\":0,\"ms\":0,\"event\":\"run_start\",\"run\":\"run-late\"}\n",
                )
                .unwrap();
                true
            } else {
                false
            }
        });
        let text = tail_to_string(&project, &opts(true, false), &mut ticker);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with("waiting for a run"), "{text}");
        assert!(lines[1].contains("run run-late"));
    }

    #[test]
    fn json_mode_never_prints_the_waiting_notice() {
        let (_d, project) = project_with_events(&[]);
        let events = project.join(".keel").join("events");
        let mut ticker = ScriptTicker::new(move |n| {
            if n == 0 {
                std::fs::write(
                    events.join("run-late.ndjson"),
                    "{\"v\":1,\"seq\":0,\"ms\":0,\"event\":\"run_start\",\"run\":\"run-late\"}\n",
                )
                .unwrap();
                true
            } else {
                false
            }
        });
        let text = tail_to_string(&project, &opts(true, true), &mut ticker);
        assert!(text.starts_with('{'), "pure NDJSON, no notices: {text}");
    }

    // ---- line hygiene ---------------------------------------------------

    #[test]
    fn malformed_and_non_object_lines_are_skipped() {
        let body = "not json at all\n\
                    {\"v\":1,\"seq\":0,\"ms\":0,\"event\":\"run_start\",\"run\":\"run-a\"}\n\
                    [1,2,3]\n\
                    \n";
        let (_d, project) = project_with_events(&[("run-a", body)]);
        let human = tail_to_string(&project, &opts(false, false), &mut no_tick());
        assert_eq!(human.lines().count(), 1);
        let ndjson = tail_to_string(&project, &opts(false, true), &mut no_tick());
        assert_eq!(ndjson.lines().count(), 1);
        assert!(ndjson.starts_with("{\"event\":\"run_start\""));
    }

    #[test]
    fn cursor_drain_is_incremental() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("run-x.ndjson");
        std::fs::write(
            &path,
            "{\"v\":1,\"seq\":0,\"ms\":0,\"event\":\"run_start\",\"run\":\"run-x\"}\n",
        )
        .unwrap();
        let mut cursor = Cursor::open(&path).unwrap();
        let o = opts(false, true);

        let mut out = Vec::new();
        cursor.drain(&o, &mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap().lines().count(), 1);

        // Nothing new → nothing rendered (no re-reads from the start).
        let mut out = Vec::new();
        cursor.drain(&o, &mut out).unwrap();
        assert!(out.is_empty());
    }
}
