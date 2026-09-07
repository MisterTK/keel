//! `keel report` — the evidence rendered as one self-contained HTML file
//! (design spec 2026-09-04, Part B).
//!
//! The data blob is assembled from the readers `status` and `tail` already
//! own — nothing new is computed from raw evidence — so the report can never
//! disagree with `keel status --json`. Modes: static export (default),
//! `--json` (print the blob), `--watch` (rewrite on an interval; the page
//! reloads itself) and `--serve` (`report_serve`: a `127.0.0.1` listener the
//! page polls). Every mode is a pure function of the evidence files plus the
//! injected `now_ms` (dx-spec §5).

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use keel_journal::{DailyStats, MS_PER_DAY};
use serde::Serialize;
use serde_json::Value;

use crate::render::to_json;
use crate::status::{self, NO_EVIDENCE, StatusReport};
use crate::tail::{self, RunInfo};
use crate::{EXIT_FAILURE, EXIT_USAGE, Rendered, evidence, report_html};

/// Blob line-format version — the `v` stamped on every blob.
pub const BLOB_VERSION: u32 = 1;
/// How many of the newest run's events the blob (and the page) carry.
pub const EVENT_LIMIT: usize = 200;
/// Where the static export lands unless `--out` says otherwise.
pub const DEFAULT_OUT: &str = ".keel/report.html";
/// `--watch`'s default rewrite interval.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(2);
/// The trailing-day window the blob's `daily` rows cover (matches `status`).
const WINDOW_DAYS: i64 = 7;

/// Which serving mode produced a blob; the page reads it to pick its banner
/// and, under `watch`, its reload timer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Static,
    Watch,
    Serve,
}

/// The report's data — the contract the page consumes (docs/report-format.md).
#[derive(Debug, Serialize)]
pub struct ReportData {
    pub v: u32,
    /// The injected `now_ms` — the only wall-clock value in the blob.
    pub generated_at_ms: i64,
    pub mode: Mode,
    pub watch_interval_ms: u64,
    /// Byte-for-byte what `keel status --json` prints.
    pub status: StatusReport,
    /// The trailing-week slice of the stored daily buckets (per target, per
    /// UTC day) — `status.week` is the sum; the page's trend needs the rows.
    pub daily: Vec<DailyStats>,
    /// The newest run, or `None` when no run has been recorded.
    pub run: Option<RunInfo>,
    /// That run's newest events (raw `tail --json` lines), oldest first.
    pub events: Vec<Value>,
    /// Highest `seq` in the run's feed — the cursor `--serve` clients resume from.
    pub events_seq: u64,
}

/// What `keel report` was asked to do (see `main.rs`'s `Command::Report`).
#[derive(Debug, Clone)]
pub struct ReportOptions {
    pub out: Option<PathBuf>,
    pub open: bool,
    pub watch: bool,
    pub interval: Duration,
    pub serve: bool,
    pub port: u16,
}

/// Build the blob, or `Ok(None)` when there is no evidence at all (no
/// discovery store, no journal, no recorded run). `since` restricts `events`
/// to lines after that `seq` (the `--serve` cursor).
pub fn assemble(
    project: &Path,
    now_ms: i64,
    mode: Mode,
    watch_interval_ms: u64,
    since: Option<u64>,
) -> Result<Option<ReportData>, Rendered> {
    let status = status::report(project, now_ms).map_err(|e| evidence_error(&e))?;
    let end_day = now_ms.div_euclid(MS_PER_DAY);
    let start_day = end_day - (WINDOW_DAYS - 1);
    let daily: Vec<DailyStats> = evidence::read_discovery_daily(project)
        .map_err(|e| evidence_error(&e))?
        .into_iter()
        .filter(|d| d.day >= start_day && d.day <= end_day)
        .collect();
    let slice = tail::read_events(project, None, since, EVENT_LIMIT)?;
    if !status.discovery_present && !status.journal_present && slice.is_none() {
        return Ok(None);
    }
    let (run, events, events_seq) = match slice {
        Some(s) => (Some(s.run), s.events, s.last_seq),
        None => (None, Vec::new(), 0),
    };
    Ok(Some(ReportData {
        v: BLOB_VERSION,
        generated_at_ms: now_ms,
        mode,
        watch_interval_ms,
        status,
        daily,
        run,
        events,
        events_seq,
    }))
}

/// `2s`, `500ms`, or a bare number of seconds. Zero is rejected.
pub fn parse_interval(text: &str) -> Result<Duration, String> {
    let t = text.trim();
    let (digits, unit_ms) = if let Some(n) = t.strip_suffix("ms") {
        (n, 1u64)
    } else if let Some(n) = t.strip_suffix('s') {
        (n, 1000)
    } else {
        (t, 1000)
    };
    let n: u64 = digits
        .trim()
        .parse()
        .map_err(|_| format!("invalid interval {text:?}: use e.g. `2s` or `500ms`"))?;
    if n == 0 {
        return Err(format!("invalid interval {text:?}: must be greater than zero"));
    }
    Ok(Duration::from_millis(n * unit_ms))
}

/// The default and `--json` modes: write the page once (or print the blob).
pub fn run_static(project: &Path, opts: &ReportOptions, now_ms: i64, json: bool) -> Rendered {
    #[derive(Serialize)]
    struct Written {
        written: String,
    }

    if json && (opts.watch || opts.serve) {
        return usage("--json cannot be combined with --watch or --serve");
    }
    let interval_ms = u64::try_from(DEFAULT_INTERVAL.as_millis()).unwrap_or(2000);
    let data = match assemble(project, now_ms, Mode::Static, interval_ms, None) {
        Ok(Some(d)) => d,
        Ok(None) => return no_evidence(),
        Err(r) => return r,
    };
    if json {
        return Rendered::ok(String::new(), to_json(&data));
    }
    let out = out_path(project, opts);
    if let Err(e) = write_atomic(&out, &report_html::render(&data)) {
        return write_error(&out, &e);
    }
    let mut human = format!("keel \u{25b8} wrote {}", out.display());
    if opts.open && !open_in_browser(&out.display().to_string()) {
        human.push_str("\n  (could not launch a browser; open the file yourself)");
    }
    Rendered::ok(human, to_json(&Written { written: out.display().to_string() }))
}

/// Where this invocation writes its HTML.
pub fn out_path(project: &Path, opts: &ReportOptions) -> PathBuf {
    opts.out.clone().unwrap_or_else(|| project.join(DEFAULT_OUT))
}

/// Write `contents` to `path` without a reader ever seeing a partial file:
/// a sibling temp file, then an atomic rename (same directory ⇒ same
/// filesystem, so `rename` is atomic on every platform Rust supports).
pub fn write_atomic(path: &Path, contents: &str) -> io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("report.html");
    let tmp = path.with_file_name(format!("{name}.tmp-{}", std::process::id()));
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)
}

/// Launch the platform's default opener, detached. `false` when it could not
/// be spawned — never an error: the caller prints the path/URL instead.
pub fn open_in_browser(target: &str) -> bool {
    let mut cmd = if cfg!(target_os = "macos") {
        let mut c = Command::new("open");
        c.arg(target);
        c
    } else if cfg!(target_os = "windows") {
        let mut c = Command::new("cmd");
        c.args(["/C", "start", "", target]);
        c
    } else {
        let mut c = Command::new("xdg-open");
        c.arg(target);
        c
    };
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .is_ok()
}

/// The `keel status` nudge, exit 0, nothing written.
pub(crate) fn no_evidence() -> Rendered {
    #[derive(Serialize)]
    struct Nudge<'a> {
        evidence: bool,
        next: &'a str,
    }
    Rendered::ok(NO_EVIDENCE, to_json(&Nudge { evidence: false, next: "keel run <script>" }))
}

/// A usage error (exit 2, stderr).
pub(crate) fn usage(message: &str) -> Rendered {
    #[derive(Serialize)]
    struct Err<'a> {
        error: &'a str,
    }
    Rendered {
        human: format!("keel \u{25b8} {message}"),
        json: to_json(&Err { error: message }),
        exit: EXIT_USAGE,
        to_stderr: true,
    }
}

/// An evidence file could not be read (exit 1, stderr) — same shape as
/// `status`'s soft error.
pub(crate) fn evidence_error(message: &str) -> Rendered {
    #[derive(Serialize)]
    struct Err<'a> {
        error: &'a str,
    }
    Rendered {
        human: format!("keel \u{25b8} report unavailable: {message}"),
        json: to_json(&Err { error: message }),
        exit: EXIT_FAILURE,
        to_stderr: true,
    }
}

/// The output path could not be written (exit 1, stderr).
pub(crate) fn write_error(path: &Path, error: &io::Error) -> Rendered {
    #[derive(Serialize)]
    struct Err {
        error: String,
        path: String,
    }
    let message = format!("could not write {}: {error}", path.display());
    Rendered {
        human: format!("keel \u{25b8} {message}"),
        json: to_json(&Err { error: message, path: path.display().to_string() }),
        exit: EXIT_FAILURE,
        to_stderr: true,
    }
}
