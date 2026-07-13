//! `keel flows` and `keel trace <flow>` — the Tier 2 durable-flow inspectors
//! (dx-spec §6; architecture spec §4.3–4.4).
//!
//! Both read only `.keel/journal.db` (the frozen `contracts/journal.sql`
//! schema), so they are pure queries any SQLite tool could reproduce:
//!
//! - `keel flows` lists each flow — id, entrypoint, status, steps done/total,
//!   age — with `--dead` narrowing to the poison flows a resume gave up on.
//! - `keel trace <flow>` walks one flow's steps in order with their outcome,
//!   attempts, and duration.
//!
//! Internal control rows never surface: the reserved seq-0 attempt counter and
//! any `flow:*` replay-branch markers (`kind = 'marker'`) are excluded from both
//! the step counts and the trace, so the user sees only real work.
//!
//! Determinism (dx-spec §5): the `--json` twin carries only values read from the
//! DB (ids, statuses, `created_at`/`updated_at` in ms) — never a wall-clock
//! "age", which lives in the human view alone. `age` is computed against an
//! injected `now`, so the human output is reproducible under test.

use std::path::Path;

use rusqlite::{Connection, OpenFlags};
use serde::Serialize;

use crate::render::to_json;
use crate::{EXIT_FAILURE, Rendered, evidence};

/// One flow row for `keel flows`.
#[derive(Debug, Serialize)]
struct FlowRow {
    created_at: i64,
    entrypoint: String,
    flow_id: String,
    status: String,
    steps_done: i64,
    steps_total: i64,
    updated_at: i64,
}

/// The `keel flows` report — one struct so the human table and `--json` cannot
/// drift.
#[derive(Debug, Serialize)]
struct FlowsReport {
    count: usize,
    dead_only: bool,
    flows: Vec<FlowRow>,
    journal_present: bool,
}

/// `keel flows [--dead]` for `project`, dating ages against `now_ms`.
pub fn flows(project: &Path, dead_only: bool, now_ms: i64) -> Rendered {
    // Honor the policy's `journal` key (file: locations), like the engine does.
    let path = evidence::resolved_journal(project).path;
    if !path.exists() {
        return Rendered::ok(
            "keel \u{25b8} no flows yet.\n  Run a flow with `keel run <script>` (a `[flows]` entrypoint) to record one."
                .to_owned(),
            to_json(&FlowsReport {
                count: 0,
                dead_only,
                flows: Vec::new(),
                journal_present: false,
            }),
        );
    }
    let rows = match read_flows(&path, dead_only) {
        Ok(r) => r,
        Err(e) => return soft_error(&e),
    };
    let report = FlowsReport {
        count: rows.len(),
        dead_only,
        flows: rows,
        journal_present: true,
    };
    let human = flows_human(&report, now_ms);
    Rendered::ok(human, to_json(&report))
}

/// Read the flows table (and per-flow step counts), newest-updated first.
fn read_flows(path: &Path, dead_only: bool) -> Result<Vec<FlowRow>, String> {
    let conn = open_ro(path)?;
    let sql = if dead_only {
        "SELECT flow_id, entrypoint, status, created_at, updated_at FROM flows \
         WHERE status = 'dead' ORDER BY updated_at DESC, flow_id"
    } else {
        "SELECT flow_id, entrypoint, status, created_at, updated_at FROM flows \
         ORDER BY updated_at DESC, flow_id"
    };
    let mut stmt = conn.prepare(sql).map_err(|e| q(&e))?;
    let raw = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })
        .map_err(|e| q(&e))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| q(&e))?;

    let mut out = Vec::with_capacity(raw.len());
    for (flow_id, entrypoint, status, created_at, updated_at) in raw {
        let (steps_total, steps_done) = step_counts(&conn, &flow_id)?;
        out.push(FlowRow {
            created_at,
            entrypoint,
            flow_id,
            status,
            steps_done,
            steps_total,
            updated_at,
        });
    }
    Ok(out)
}

/// `(total, done)` real steps for a flow — excluding internal `marker` rows (the
/// seq-0 attempt counter and replay-branch markers). `done` counts terminal
/// (`ok`/`error`) outcomes; a still-`running` step is counted only in `total`.
fn step_counts(conn: &Connection, flow_id: &str) -> Result<(i64, i64), String> {
    conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(CASE WHEN outcome IN ('ok','error') THEN 1 ELSE 0 END), 0) \
         FROM steps WHERE flow_id = ?1 AND kind != 'marker'",
        [flow_id],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
    )
    .map_err(|e| q(&e))
}

/// The human flows table, derived entirely from [`FlowsReport`].
fn flows_human(report: &FlowsReport, now_ms: i64) -> String {
    if report.flows.is_empty() {
        let what = if report.dead_only {
            "no dead flows \u{2014} nothing has exhausted its resume cap."
        } else {
            "no flows recorded yet."
        };
        return format!("keel \u{25b8} {what}");
    }
    let scope = if report.dead_only { " (dead)" } else { "" };
    let mut lines = vec![format!(
        "keel \u{25b8} flows{scope}: {} total\n",
        report.count
    )];
    for f in &report.flows {
        lines.push(format!(
            "  {}  {}  {}  steps {}/{}  {}\n",
            f.flow_id,
            f.entrypoint,
            f.status,
            f.steps_done,
            f.steps_total,
            fmt_age(now_ms, f.updated_at),
        ));
    }
    lines.concat()
}

/// One step line for `keel trace`.
#[derive(Debug, Serialize)]
struct TraceStep {
    attempt: i64,
    duration_ms: Option<i64>,
    ended_at: Option<i64>,
    kind: String,
    outcome: String,
    seq: i64,
    started_at: i64,
    step_key: String,
}

/// The `keel trace <flow>` report.
#[derive(Debug, Serialize)]
struct TraceReport {
    created_at: i64,
    entrypoint: String,
    flow_id: String,
    status: String,
    steps: Vec<TraceStep>,
    updated_at: i64,
}

/// `keel trace <flow>` for `project`. `flow` is an exact `flow_id`, or a
/// substring of an id/entrypoint that resolves to exactly one flow.
pub fn trace(project: &Path, flow: &str) -> Rendered {
    let path = evidence::resolved_journal(project).path;
    if !path.exists() {
        return soft_error("no journal yet (.keel/journal.db). Run a flow first with `keel run`.");
    }
    let conn = match open_ro(&path) {
        Ok(c) => c,
        Err(e) => return soft_error(&e),
    };
    let resolved = match resolve_flow(&conn, flow) {
        Ok(r) => r,
        Err(e) => return soft_error(&e),
    };
    let steps = match read_steps(&conn, &resolved.flow_id) {
        Ok(s) => s,
        Err(e) => return soft_error(&e),
    };
    let report = TraceReport {
        created_at: resolved.created_at,
        entrypoint: resolved.entrypoint,
        flow_id: resolved.flow_id,
        status: resolved.status,
        steps,
        updated_at: resolved.updated_at,
    };
    let human = trace_human(&report);
    Rendered::ok(human, to_json(&report))
}

/// A resolved flow header (before its steps are read). Shared with
/// [`replay`](crate::replay), which resolves flows the same way.
pub(crate) struct ResolvedFlow {
    pub(crate) flow_id: String,
    pub(crate) entrypoint: String,
    pub(crate) status: String,
    pub(crate) created_at: i64,
    pub(crate) updated_at: i64,
}

/// Resolve `flow` to exactly one flow: an exact `flow_id`, else a unique
/// substring match on id or entrypoint. Ambiguity and absence are precise
/// errors, never a silent pick.
pub(crate) fn resolve_flow(conn: &Connection, flow: &str) -> Result<ResolvedFlow, String> {
    let like = format!("%{flow}%");
    let mut stmt = conn
        .prepare(
            "SELECT flow_id, entrypoint, status, created_at, updated_at FROM flows \
             WHERE flow_id = ?1 OR flow_id LIKE ?2 OR entrypoint LIKE ?2 \
             ORDER BY (flow_id = ?1) DESC, updated_at DESC, flow_id",
        )
        .map_err(|e| q(&e))?;
    let matches = stmt
        .query_map([flow, &like], |row| {
            Ok(ResolvedFlow {
                flow_id: row.get(0)?,
                entrypoint: row.get(1)?,
                status: row.get(2)?,
                created_at: row.get(3)?,
                updated_at: row.get(4)?,
            })
        })
        .map_err(|e| q(&e))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| q(&e))?;

    // An exact id match wins outright even if the substring also hits others.
    if let Some(first) = matches.first()
        && first.flow_id == flow
    {
        return Ok(matches.into_iter().next().expect("first exists"));
    }
    match matches.len() {
        0 => Err(format!(
            "no flow matches {flow:?} in the journal. Run `keel flows` to list recorded flows."
        )),
        1 => Ok(matches.into_iter().next().expect("one match")),
        n => {
            let ids: Vec<&str> = matches.iter().take(5).map(|m| m.flow_id.as_str()).collect();
            Err(format!(
                "{flow:?} matches {n} flows ({}); use a full flow_id.",
                ids.join(", ")
            ))
        }
    }
}

/// Read a flow's real steps (excluding `marker` rows) in seq order.
fn read_steps(conn: &Connection, flow_id: &str) -> Result<Vec<TraceStep>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT seq, step_key, kind, attempt, outcome, started_at, ended_at FROM steps \
             WHERE flow_id = ?1 AND kind != 'marker' ORDER BY seq",
        )
        .map_err(|e| q(&e))?;
    let steps = stmt
        .query_map([flow_id], |row| {
            let seq: i64 = row.get(0)?;
            let step_key: String = row.get(1)?;
            let kind: String = row.get(2)?;
            let attempt: i64 = row.get(3)?;
            let outcome: String = row.get(4)?;
            let started_at: i64 = row.get(5)?;
            let ended_at: Option<i64> = row.get(6)?;
            Ok(TraceStep {
                attempt,
                duration_ms: ended_at.map(|e| e - started_at),
                ended_at,
                kind,
                outcome,
                seq,
                started_at,
                step_key,
            })
        })
        .map_err(|e| q(&e))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| q(&e))?;
    Ok(steps)
}

/// The human trace, derived entirely from [`TraceReport`].
fn trace_human(report: &TraceReport) -> String {
    let mut lines = vec![
        format!(
            "keel \u{25b8} trace {}\n  entrypoint: {}\n  status:     {}\n",
            report.flow_id, report.entrypoint, report.status
        ),
        format!("  steps:      {}\n", report.steps.len()),
    ];
    for s in &report.steps {
        let dur = match s.duration_ms {
            Some(ms) => format!("{ms}ms"),
            None => "\u{2014}".to_owned(), // still running
        };
        let attempts = if s.attempt > 1 {
            format!(" ({} attempts)", s.attempt)
        } else {
            String::new()
        };
        lines.push(format!(
            "    {:>3}. {:<7} {:<28} {:<8} {}{}\n",
            s.seq, s.kind, s.step_key, s.outcome, dur, attempts,
        ));
    }
    lines.concat()
}

/// A coarse human age ("5s", "3m", "2h", "1d") from `then_ms` to `now_ms`.
fn fmt_age(now_ms: i64, then_ms: i64) -> String {
    let secs = (now_ms - then_ms).max(0) / 1000;
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

pub(crate) fn open_ro(path: &Path) -> Result<Connection, String> {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| format!("could not open {}: {e}", path.display()))
}

pub(crate) fn q(e: &rusqlite::Error) -> String {
    format!("journal query failed: {e}")
}

/// A read failure (exit 1, on stderr) — mirrors `keel status`. Shared with
/// [`crate::replay`] (the same journal reads) and `keel flows suggest` (same
/// failure mode there: a corrupt `.keel/discovery.db`).
pub(crate) fn soft_error(message: &str) -> Rendered {
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
    use std::path::PathBuf;

    const T0: i64 = 1_783_728_000_000;

    /// Build a project dir with `.keel/journal.db` from a golden fixture
    /// (`conformance/fixtures/journal/<fixture>`), applied over the frozen
    /// schema — self-contained, no reliance on the checked-in `.gen/` build.
    fn project_with_fixture(fixture: &str) -> (tempfile::TempDir, PathBuf) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let schema = std::fs::read_to_string(root.join("contracts/journal.sql")).unwrap();
        let sql = std::fs::read_to_string(root.join("conformance/fixtures/journal").join(fixture))
            .unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let keel = dir.path().join(".keel");
        std::fs::create_dir_all(&keel).unwrap();
        let db = keel.join("journal.db");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(&schema).unwrap();
        conn.execute_batch(&sql).unwrap();
        let project = dir.path().to_path_buf();
        (dir, project)
    }

    #[test]
    fn flows_lists_completed_with_step_counts() {
        let (_d, project) = project_with_fixture("completed-flow.sql");
        let r = flows(&project, false, T0 + 10_000);
        assert_eq!(r.exit, crate::EXIT_OK);
        assert_eq!(r.json["count"], 1);
        let f = &r.json["flows"][0];
        assert_eq!(f["entrypoint"], "py:pipeline.ingest:main");
        assert_eq!(f["status"], "completed");
        assert_eq!(f["steps_done"], 5);
        assert_eq!(f["steps_total"], 5);
        // Human view is deterministic under a fixed `now`.
        assert!(r.human.contains("steps 5/5"));
        assert!(r.human.contains("ago"));
    }

    #[test]
    fn flows_interrupted_shows_incomplete_step_count() {
        let (_d, project) = project_with_fixture("interrupted-flow.sql");
        let r = flows(&project, false, T0 + 60_000);
        let f = &r.json["flows"][0];
        assert_eq!(f["status"], "running");
        // 4 recorded steps; step 4 is still `running` → 3 done of 4.
        assert_eq!(f["steps_total"], 4);
        assert_eq!(f["steps_done"], 3);
    }

    #[test]
    fn flows_dead_filter_selects_only_dead() {
        let (_d, project) = project_with_fixture("dead-flow.sql");
        let all = flows(&project, false, T0);
        assert_eq!(all.json["count"], 1);
        let dead = flows(&project, true, T0);
        assert_eq!(dead.json["count"], 1);
        assert_eq!(dead.json["dead_only"], true);
        assert_eq!(dead.json["flows"][0]["status"], "dead");
        // A completed fixture has no dead flows.
        let (_d2, p2) = project_with_fixture("completed-flow.sql");
        assert_eq!(flows(&p2, true, T0).json["count"], 0);
    }

    #[test]
    fn flows_absent_journal_nudges() {
        let dir = tempfile::TempDir::new().unwrap();
        let r = flows(dir.path(), false, T0);
        assert_eq!(r.exit, crate::EXIT_OK);
        assert_eq!(r.json["journal_present"], false);
        assert_eq!(r.json["count"], 0);
    }

    #[test]
    fn trace_walks_steps_with_outcomes_and_durations() {
        let (_d, project) = project_with_fixture("completed-flow.sql");
        let r = trace(&project, "py:pipeline.ingest:main");
        assert_eq!(r.exit, crate::EXIT_OK);
        assert_eq!(r.json["status"], "completed");
        let steps = r.json["steps"].as_array().unwrap();
        assert_eq!(steps.len(), 5);
        // seq 1: the fetch effect, 240ms.
        assert_eq!(steps[0]["seq"], 1);
        assert_eq!(steps[0]["step_key"], "api.source.internal#q1");
        assert_eq!(steps[0]["outcome"], "ok");
        assert_eq!(steps[0]["duration_ms"], 240);
        // seq 2: the virtualized time read.
        assert_eq!(steps[1]["kind"], "time");
        assert_eq!(steps[1]["step_key"], "py:time.time#-");
        // seq 3: enrich succeeded on its 2nd attempt.
        assert_eq!(steps[2]["attempt"], 2);
        assert!(r.human.contains("2 attempts"));
    }

    #[test]
    fn trace_running_step_has_no_duration() {
        let (_d, project) = project_with_fixture("interrupted-flow.sql");
        let r = trace(&project, "01JZWY0A0000000000000002");
        let steps = r.json["steps"].as_array().unwrap();
        assert_eq!(steps.len(), 4);
        assert_eq!(steps[3]["outcome"], "running");
        assert!(steps[3]["duration_ms"].is_null());
        assert!(steps[3]["ended_at"].is_null());
    }

    #[test]
    fn trace_unknown_flow_is_a_soft_error() {
        let (_d, project) = project_with_fixture("completed-flow.sql");
        let r = trace(&project, "does-not-exist");
        assert_eq!(r.exit, EXIT_FAILURE);
        assert!(r.to_stderr);
        assert!(r.human.contains("no flow matches"));
    }
}
