//! `keel status` — the "what is Keel doing for me" screen (dx-spec §6).
//!
//! One screen, from the two evidence files when present: `.keel/discovery.db`
//! (wrapped coverage, calls, retries, cache hit rate, breaker/throttle events)
//! and `.keel/journal.db` (the flows table — how many ran, completed, and how
//! many are resumable after a crash). Reads only; the schema is frozen
//! (`contracts/journal.sql`). No evidence yet → a friendly nudge, exit 0.

use std::path::Path;

use keel_journal::TargetStats;
use rusqlite::{Connection, OpenFlags};
use serde::Serialize;

use crate::render::to_json;
use crate::{EXIT_FAILURE, Rendered, evidence};

/// Per-target line in the status report.
#[derive(Debug, Serialize)]
struct TargetLine {
    breaker_opens: i64,
    cache_hits: i64,
    calls: i64,
    failures: i64,
    retries: i64,
    successes: i64,
    target: String,
    throttled: i64,
}

/// The flows-table summary.
#[derive(Debug, Default, Serialize)]
struct FlowSummary {
    completed: i64,
    dead: i64,
    failed: i64,
    resumable: i64,
    running: i64,
    total: i64,
}

/// The whole status report — one struct, so the human screen and the `--json`
/// twin cannot drift (every human fact has a JSON counterpart).
#[derive(Debug, Serialize)]
struct StatusReport {
    breaker_opens: i64,
    cache_hit_rate: f64,
    cache_hits: i64,
    calls: i64,
    discovery_present: bool,
    failures: i64,
    flows: FlowSummary,
    journal_present: bool,
    retries: i64,
    success_rate: f64,
    successes: i64,
    targets: Vec<TargetLine>,
    targets_wrapped: usize,
    throttled: i64,
}

/// Build the status report for `project`.
pub fn run(project: &Path) -> Rendered {
    let discovery = match evidence::read_discovery(project) {
        Ok(d) => d,
        Err(e) => return soft_error(&e),
    };
    let discovery_present = evidence::discovery_db(project).exists();
    let journal_present = evidence::journal_db(project).exists();

    let flows = if journal_present {
        match read_flows(&evidence::journal_db(project)) {
            Ok(f) => f,
            Err(e) => return soft_error(&e),
        }
    } else {
        FlowSummary::default()
    };

    let report = aggregate(discovery, flows, discovery_present, journal_present);
    let human = human(&report);
    Rendered::ok(human, to_json(&report))
}

/// Fold per-target stats + the flow summary into the report.
fn aggregate(
    discovery: Vec<TargetStats>,
    flows: FlowSummary,
    discovery_present: bool,
    journal_present: bool,
) -> StatusReport {
    let mut r = StatusReport {
        breaker_opens: 0,
        cache_hit_rate: 0.0,
        cache_hits: 0,
        calls: 0,
        discovery_present,
        failures: 0,
        flows,
        journal_present,
        retries: 0,
        success_rate: 0.0,
        successes: 0,
        targets: Vec::new(),
        targets_wrapped: discovery.len(),
        throttled: 0,
    };
    for s in discovery {
        r.calls += s.calls;
        r.retries += s.retries;
        r.successes += s.successes;
        r.failures += s.failures;
        r.cache_hits += s.cache_hits;
        r.throttled += s.throttled;
        r.breaker_opens += s.breaker_opens;
        r.targets.push(TargetLine {
            breaker_opens: s.breaker_opens,
            cache_hits: s.cache_hits,
            calls: s.calls,
            failures: s.failures,
            retries: s.retries,
            successes: s.successes,
            target: s.target,
            throttled: s.throttled,
        });
    }
    r.cache_hit_rate = ratio(r.cache_hits, r.calls);
    r.success_rate = ratio(r.successes, r.successes + r.failures);
    r
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
        return "keel \u{25b8} no evidence yet.\n  Run `keel run <script>` to start recording coverage and flows.".to_owned();
    }
    let mut lines: Vec<String> = vec![
        "keel \u{25b8} status\n".to_owned(),
        format!(
            "  wrapped targets:  {}\n  calls:            {} ({} ok, {} failed, {} cached)\n  retries:          {}\n",
            r.targets_wrapped, r.calls, r.successes, r.failures, r.cache_hits, r.retries,
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
        format!(
            "  flows:            {} total ({} completed, {} running, {} failed, {} dead)\n  resumable:        {}\n",
            r.flows.total,
            r.flows.completed,
            r.flows.running,
            r.flows.failed,
            r.flows.dead,
            r.flows.resumable,
        ),
    ];
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
    use super::*;

    #[test]
    fn empty_project_nudges_to_run() {
        let dir = tempfile::TempDir::new().unwrap();
        let r = run(dir.path());
        assert_eq!(r.exit, crate::EXIT_OK);
        assert!(r.human.contains("no evidence yet"));
        assert_eq!(r.json["discovery_present"], false);
        assert_eq!(r.json["journal_present"], false);
    }

    #[test]
    fn ratio_rounds_and_guards_zero_denominator() {
        assert!((ratio(1, 3) - 0.3333).abs() < f64::EPSILON);
        assert!((ratio(0, 0) - 0.0).abs() < f64::EPSILON);
        assert!((ratio(1, 2) - 0.5).abs() < f64::EPSILON);
    }
}
