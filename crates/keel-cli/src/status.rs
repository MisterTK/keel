//! `keel status` — the "what is Keel doing for me" screen (dx-spec §6).
//!
//! One screen, from the two evidence files when present: `.keel/discovery.db`
//! (wrapped coverage, calls, retries, cache hit rate, breaker/throttle events,
//! the observed-not-retried and unwrapped-coverage gaps, and a trailing-7-day
//! window) and `.keel/journal.db` (the flows table — how many ran, completed,
//! and how many are resumable after a crash). Reads only; the journal schema
//! is frozen (`contracts/journal.sql`); the discovery schema is
//! `keel-journal`'s own (not contract-frozen). No evidence yet → a friendly
//! nudge, exit 0.
//!
//! Determinism (dx-spec §5): the trailing week window is computed from
//! *stored* daily buckets keyed on `now_ms`'s UTC day — never a wall-clock
//! label baked into the JSON — so `--json` output is a pure function of the
//! evidence file plus the injected `now_ms` (mirrors `flows`' `fmt_age`).

use std::path::Path;

use keel_journal::{Activation, DailyStats, MS_PER_DAY, TargetStats};
use rusqlite::{Connection, OpenFlags};
use serde::Serialize;

use crate::render::to_json;
use crate::{EXIT_FAILURE, Rendered, evidence};

/// How many trailing UTC days "this week" covers (today plus the 6 before it).
const WINDOW_DAYS: i64 = 7;

/// Per-target line in the status report.
#[derive(Debug, Serialize)]
pub struct TargetLine {
    pub breaker_opens: i64,
    pub cache_hits: i64,
    pub calls: i64,
    pub failures: i64,
    pub not_retried: i64,
    pub retries: i64,
    pub successes: i64,
    pub target: String,
    pub throttled: i64,
    pub unwrapped_calls: i64,
}

/// The flows-table summary.
#[derive(Debug, Default, Serialize)]
pub struct FlowSummary {
    pub completed: i64,
    pub dead: i64,
    pub failed: i64,
    pub resumable: i64,
    pub running: i64,
    pub total: i64,
}

/// Trailing-window aggregates over the last [`WINDOW_DAYS`] stored daily
/// buckets, ending on the day `now_ms` falls in (dx-spec §6, "retries saved
/// this week"). Zero on a legacy (v1) discovery file, which has no buckets.
#[derive(Debug, Default, Serialize)]
pub struct WeekSummary {
    pub breaker_opens: i64,
    pub cache_hits: i64,
    pub calls: i64,
    pub failures: i64,
    pub not_retried: i64,
    pub retries: i64,
    pub successes: i64,
    pub throttled: i64,
    pub unwrapped_calls: i64,
}

/// The last recorded activation, plus how many are on record — the "did
/// Keel actually run, and with which policy" evidence (#92). `count` is the
/// total number of activation rows ever recorded; `last` is the newest one.
#[derive(Debug, Serialize)]
pub struct ActivationSummary {
    pub count: usize,
    pub last: Option<Activation>,
}

/// Fold recorded activation rows (newest-first, as the store returns them)
/// into a summary.
fn summarize_activations(mut rows: Vec<Activation>) -> ActivationSummary {
    let count = rows.len();
    let last = rows.drain(..).next();
    ActivationSummary { count, last }
}

/// The whole status report — one struct, so the human screen and the `--json`
/// twin cannot drift (every human fact has a JSON counterpart).
#[derive(Debug, Serialize)]
pub struct StatusReport {
    pub activation: ActivationSummary,
    pub breaker_opens: i64,
    pub cache_hit_rate: f64,
    pub cache_hits: i64,
    pub calls: i64,
    pub discovery_present: bool,
    pub failures: i64,
    pub flows: FlowSummary,
    pub journal_present: bool,
    pub not_retried: i64,
    pub retries: i64,
    pub success_rate: f64,
    pub successes: i64,
    pub targets: Vec<TargetLine>,
    pub targets_wrapped: usize,
    pub throttled: i64,
    pub unwrapped_calls: i64,
    pub week: WeekSummary,
    pub wrapped_coverage: f64,
}

/// Build the status report for `project`, windowing "this week" against
/// `now_ms` (the caller's clock — `SystemClock` in production, a fixed value
/// under test). The one constructor both `keel status` and `keel report`
/// use, so their `status` JSON can never drift.
pub fn report(project: &Path, now_ms: i64) -> Result<StatusReport, String> {
    let discovery = evidence::read_discovery(project)?;
    let daily = evidence::read_discovery_daily(project)?;
    let activation = summarize_activations(evidence::read_activations(project)?);
    let discovery_present = evidence::discovery_db(project).exists();
    // Honor the policy's `journal` key (file: locations), like the engine does.
    let journal_path = evidence::resolved_journal(project).path;
    let journal_present = journal_path.exists();

    let flows = if journal_present {
        read_flows(&journal_path)?
    } else {
        FlowSummary::default()
    };

    Ok(aggregate(
        discovery,
        &daily,
        flows,
        discovery_present,
        journal_present,
        now_ms,
        activation,
    ))
}

/// Render `keel status` for `project` (see [`report`]).
pub fn run(project: &Path, now_ms: i64) -> Rendered {
    match report(project, now_ms) {
        Ok(r) => Rendered::ok(human(&r), to_json(&r)),
        Err(e) => soft_error(&e),
    }
}

/// The friendly nudge printed when there is no evidence yet — shared with
/// `keel report`, which must say exactly the same thing.
pub const NO_EVIDENCE: &str = "keel \u{25b8} no evidence yet.\n  Run `keel run <script>` to start recording coverage and flows.";

/// Fold per-target stats, the daily buckets, and the flow summary into the
/// report.
fn aggregate(
    discovery: Vec<TargetStats>,
    daily: &[DailyStats],
    flows: FlowSummary,
    discovery_present: bool,
    journal_present: bool,
    now_ms: i64,
    activation: ActivationSummary,
) -> StatusReport {
    let mut r = StatusReport {
        activation,
        breaker_opens: 0,
        cache_hit_rate: 0.0,
        cache_hits: 0,
        calls: 0,
        discovery_present,
        failures: 0,
        flows,
        journal_present,
        not_retried: 0,
        retries: 0,
        success_rate: 0.0,
        successes: 0,
        targets: Vec::new(),
        targets_wrapped: discovery.len(),
        throttled: 0,
        unwrapped_calls: 0,
        week: WeekSummary::default(),
        wrapped_coverage: 0.0,
    };
    for s in discovery {
        r.calls += s.calls;
        r.retries += s.retries;
        r.successes += s.successes;
        r.failures += s.failures;
        r.cache_hits += s.cache_hits;
        r.throttled += s.throttled;
        r.breaker_opens += s.breaker_opens;
        r.not_retried += s.not_retried;
        r.unwrapped_calls += s.unwrapped_calls;
        r.targets.push(TargetLine {
            breaker_opens: s.breaker_opens,
            cache_hits: s.cache_hits,
            calls: s.calls,
            failures: s.failures,
            not_retried: s.not_retried,
            retries: s.retries,
            successes: s.successes,
            target: s.target,
            throttled: s.throttled,
            unwrapped_calls: s.unwrapped_calls,
        });
    }
    r.cache_hit_rate = ratio(r.cache_hits, r.calls);
    r.success_rate = ratio(r.successes, r.successes + r.failures);
    r.wrapped_coverage = ratio(r.calls - r.unwrapped_calls, r.calls);
    r.week = week_window(daily, now_ms);
    r
}

/// Sum the daily buckets stored for the trailing [`WINDOW_DAYS`] UTC days
/// ending on `now_ms`'s day — a pure function of *stored* days, never a
/// wall-clock label, so `--json` stays byte-deterministic under an injected
/// clock (dx-spec §5).
fn week_window(daily: &[DailyStats], now_ms: i64) -> WeekSummary {
    let end_day = now_ms.div_euclid(MS_PER_DAY);
    let start_day = end_day - (WINDOW_DAYS - 1);
    let mut w = WeekSummary::default();
    for d in daily {
        if d.day < start_day || d.day > end_day {
            continue;
        }
        w.calls += d.calls;
        w.retries += d.retries;
        w.successes += d.successes;
        w.failures += d.failures;
        w.cache_hits += d.cache_hits;
        w.throttled += d.throttled;
        w.breaker_opens += d.breaker_opens;
        w.not_retried += d.not_retried;
        w.unwrapped_calls += d.unwrapped_calls;
    }
    w
}

/// A rate rounded to 4 decimals so the JSON twin is byte-stable.
fn ratio(num: i64, denom: i64) -> f64 {
    if denom <= 0 {
        return 0.0;
    }
    #[expect(clippy::cast_precision_loss, reason = "counts are small; 4-dp rounded")]
    let raw = num as f64 / denom as f64;
    (raw * 10_000.0).round() / 10_000.0
}

/// Count flows by status. `resumable` = flows in `running` (the recovery set;
/// `SqliteJournal::incomplete_flows` treats only `running` as resumable).
fn read_flows(path: &Path) -> Result<FlowSummary, String> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| format!("could not open {}: {e}", path.display()))?;
    let mut stmt = conn
        .prepare("SELECT status, COUNT(*) FROM flows GROUP BY status")
        .map_err(|e| format!("could not read flows: {e}"))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .map_err(|e| format!("could not read flows: {e}"))?;
    let mut f = FlowSummary::default();
    for row in rows {
        let (status, count) = row.map_err(|e| format!("could not read flows: {e}"))?;
        match status.as_str() {
            "running" => f.running = count,
            "completed" => f.completed = count,
            "failed" => f.failed = count,
            "dead" => f.dead = count,
            _ => {}
        }
        f.total += count;
    }
    f.resumable = f.running;
    Ok(f)
}

/// The human screen. Derived entirely from [`StatusReport`], so it can never
/// show a fact the JSON twin omits.
fn human(r: &StatusReport) -> String {
    if !r.discovery_present && !r.journal_present {
        return NO_EVIDENCE.to_owned();
    }
    let mut lines: Vec<String> = vec![
        "keel \u{25b8} status\n".to_owned(),
        format!(
            "  wrapped targets:  {} ({:.1}% of calls covered by policy)\n  calls:            {} ({} ok, {} failed, {} cached)\n  retries:          {} ({} saved this week)\n",
            r.targets_wrapped,
            r.wrapped_coverage * 100.0,
            r.calls,
            r.successes,
            r.failures,
            r.cache_hits,
            r.retries,
            r.week.retries,
        ),
        format!(
            "  success rate:     {:.1}%\n  cache hit rate:   {:.1}%\n",
            r.success_rate * 100.0,
            r.cache_hit_rate * 100.0,
        ),
        format!(
            "  breaker events:   {}\n  throttled:        {}\n",
            r.breaker_opens, r.throttled,
        ),
    ];
    if let Some(a) = &r.activation.last {
        let policy = match (&a.policy_source[..], &a.policy_path) {
            ("defaults", _) => format!("production defaults (no keel.toml in {})", a.cwd),
            (_, Some(p)) => format!("policy {p}"),
            (_, None) => "policy keel.toml".to_owned(),
        };
        // `backend` (#129) is absent on a row written before the column
        // existed, so the suffix is omitted rather than printing "backend
        // unknown" — byte-identical to the pre-#129 line in that case.
        let backend = a
            .backend
            .as_deref()
            .map(|b| format!(" \u{b7} backend {b}"))
            .unwrap_or_default();
        lines.push(format!(
            "  last activation:  {} {} \u{b7} {}{} \u{b7} pid {}\n",
            a.language, a.version, policy, backend, a.pid,
        ));
    }
    lines.push(format!(
        "  flows:            {} total ({} completed, {} running, {} failed, {} dead)\n  resumable:        {}\n",
        r.flows.total,
        r.flows.completed,
        r.flows.running,
        r.flows.failed,
        r.flows.dead,
        r.flows.resumable,
    ));
    if r.unwrapped_calls > 0 {
        lines.push(format!(
            "  coverage gap:     {} call(s) observed on targets with no policy entry — run `keel init` to add them.\n",
            r.unwrapped_calls,
        ));
    }
    let not_retried_targets: Vec<&str> = r
        .targets
        .iter()
        .filter(|t| t.not_retried > 0)
        .map(|t| t.target.as_str())
        .collect();
    if !not_retried_targets.is_empty() {
        lines.push(format!(
            "  observed, not retried: {} call(s) on {} — non-idempotent (no idempotency key), so Keel will not retry them by default; add `idempotency.header` in keel.toml to allow it.\n",
            r.not_retried,
            not_retried_targets.join(", "),
        ));
    }
    if !r.targets.is_empty() {
        lines.push("  by target:\n".to_owned());
        for t in &r.targets {
            lines.push(format!(
                "    {}  ({} calls, {} retries, {} cache hits)\n",
                t.target, t.calls, t.retries, t.cache_hits,
            ));
        }
    }
    lines.concat()
}

/// An evidence-read failure (exit 1: the underlying data could not be read).
fn soft_error(message: &str) -> Rendered {
    #[derive(Serialize)]
    struct ErrReport<'a> {
        error: &'a str,
    }
    Rendered {
        human: format!("keel \u{25b8} status unavailable: {message}"),
        json: to_json(&ErrReport { error: message }),
        exit: EXIT_FAILURE,
        to_stderr: true,
    }
}

#[cfg(test)]
mod tests {
    use keel_journal::{CallObservation, CallResult, DiscoveryStore, ManualClock, ObservedError};

    use super::*;

    const T0: i64 = 1_783_728_000_000; // an arbitrary but fixed UTC instant

    #[test]
    fn activation_summary_reports_count_and_newest() {
        let a = |ts: i64| Activation {
            ts_ms: ts,
            pid: 1,
            language: "python".into(),
            version: "0.5.6".into(),
            cwd: "/code".into(),
            keel_cwd: None,
            policy_source: "keel.toml".into(),
            policy_path: Some("/code/keel.toml".into()),
            flows_configured: false,
            argv0: "app.py".into(),
            backend: Some("native".into()),
        };
        let s = summarize_activations(vec![a(20), a(10)]);
        assert_eq!(s.count, 2);
        assert_eq!(s.last.as_ref().map(|x| x.ts_ms), Some(20));
        assert_eq!(summarize_activations(vec![]).count, 0);
    }

    #[test]
    fn human_status_prints_the_last_activation_line() {
        // Build a StatusReport via `aggregate` with one activation; assert the line.
        let r = aggregate(
            vec![],
            &[],
            FlowSummary::default(),
            true,
            false,
            T0,
            summarize_activations(vec![Activation {
                ts_ms: 1,
                pid: 4242,
                language: "python".into(),
                version: "0.5.6".into(),
                cwd: "/code".into(),
                keel_cwd: Some("/code".into()),
                policy_source: "defaults".into(),
                policy_path: None,
                flows_configured: false,
                argv0: String::new(),
                backend: Some("native".into()),
            }]),
        );
        assert!(
            human(&r).contains(
                "  last activation:  python 0.5.6 \u{b7} production defaults (no keel.toml in /code) \u{b7} backend native \u{b7} pid 4242\n"
            ),
            "{}",
            human(&r)
        );
    }

    /// A row written before #129 (or read back from a v3 file with no
    /// `backend` column — see `keel_journal::discovery`) has `backend: None`;
    /// the line must stay byte-identical to the pre-#129 form rather than
    /// printing a "backend unknown" placeholder.
    #[test]
    fn human_status_omits_the_backend_segment_when_absent() {
        let r = aggregate(
            vec![],
            &[],
            FlowSummary::default(),
            true,
            false,
            T0,
            summarize_activations(vec![Activation {
                ts_ms: 1,
                pid: 4242,
                language: "python".into(),
                version: "0.5.6".into(),
                cwd: "/code".into(),
                keel_cwd: Some("/code".into()),
                policy_source: "defaults".into(),
                policy_path: None,
                flows_configured: false,
                argv0: String::new(),
                backend: None,
            }]),
        );
        assert!(
            human(&r).contains(
                "  last activation:  python 0.5.6 \u{b7} production defaults (no keel.toml in /code) \u{b7} pid 4242\n"
            ),
            "{}",
            human(&r)
        );
    }

    #[test]
    fn empty_project_nudges_to_run() {
        let dir = tempfile::TempDir::new().unwrap();
        let r = run(dir.path(), T0);
        assert_eq!(r.exit, crate::EXIT_OK);
        assert!(r.human.contains("no evidence yet"));
        assert_eq!(r.json["discovery_present"], false);
        assert_eq!(r.json["journal_present"], false);
    }

    #[test]
    fn report_and_run_agree_byte_for_byte() {
        let dir = tempfile::TempDir::new().unwrap();
        // No evidence at all: both paths must still agree on the JSON twin.
        let via_run = run(dir.path(), 1_783_728_000_000).json;
        let via_report = to_json(&report(dir.path(), 1_783_728_000_000).unwrap());
        assert_eq!(via_run, via_report);
        assert!(!via_report["discovery_present"].as_bool().unwrap());
    }

    #[test]
    fn ratio_rounds_and_guards_zero_denominator() {
        assert!((ratio(1, 3) - 0.3333).abs() < f64::EPSILON);
        assert!((ratio(0, 0) - 0.0).abs() < f64::EPSILON);
        assert!((ratio(1, 2) - 0.5).abs() < f64::EPSILON);
    }

    fn observation(target: &str, not_retried: bool, wrapped: bool) -> CallObservation {
        CallObservation {
            target: target.to_owned(),
            result: if not_retried {
                CallResult::Failure
            } else {
                CallResult::Success
            },
            attempts: if not_retried { 1 } else { 2 },
            latency_ms: 10,
            throttled: false,
            breaker_opened: false,
            not_retried,
            wrapped,
            error: not_retried.then_some(ObservedError {
                class: keel_journal::ErrorClass::Http,
                http_status: Some(500),
            }),
        }
    }

    #[test]
    fn coverage_gap_and_not_retried_surface_in_the_report() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".keel")).unwrap();
        let clock = ManualClock::new(T0);
        {
            let store =
                DiscoveryStore::open(dir.path().join(".keel").join("discovery.db"), clock).unwrap();
            store
                .record(&observation("api.stripe.com", true, true))
                .unwrap();
            store
                .record(&observation("api.unconfigured.com", false, false))
                .unwrap();
        }

        let r = run(dir.path(), T0);
        assert_eq!(r.exit, crate::EXIT_OK);
        assert_eq!(r.json["not_retried"], 1);
        assert_eq!(r.json["unwrapped_calls"], 1);
        assert!(
            (r.json["wrapped_coverage"].as_f64().unwrap() - 0.5).abs() < f64::EPSILON,
            "1 of 2 calls came from a target with no policy entry"
        );
        assert!(r.human.contains("observed, not retried"));
        assert!(r.human.contains("api.stripe.com"));
        assert!(r.human.contains("coverage gap"));
    }

    #[test]
    fn week_window_sums_only_the_trailing_seven_stored_days() {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".keel")).unwrap();
        let clock = ManualClock::new(T0);
        let db = dir.path().join(".keel").join("discovery.db");
        {
            let store = DiscoveryStore::open(&db, clock.clone()).unwrap();
            store.record(&observation("api.x", false, true)).unwrap(); // day 0, in window
            clock.advance(6 * MS_PER_DAY);
            store.record(&observation("api.x", false, true)).unwrap(); // day 6, in window (edge)
            clock.advance(MS_PER_DAY); // day 7: now outside a window ending at day 6
            store.record(&observation("api.x", false, true)).unwrap(); // recorded so day 7 exists,
            // but a report as-of day 6 must not see it.
        }

        let as_of_day_6 = T0 + 6 * MS_PER_DAY;
        let r = run(dir.path(), as_of_day_6);
        // Window is [day 0, day 6] inclusive: 2 of the 3 recorded calls.
        assert_eq!(r.json["week"]["calls"], 2);
        assert_eq!(
            r.json["calls"], 3,
            "lifetime total is unaffected by windowing"
        );

        let as_of_day_7 = T0 + 7 * MS_PER_DAY;
        let r7 = run(dir.path(), as_of_day_7);
        // Window slides to [day 1, day 7]: day 0's call falls out, day 7's falls in.
        assert_eq!(r7.json["week"]["calls"], 2);
    }
}
