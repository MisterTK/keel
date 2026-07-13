//! `keel replay <flow>` — journal-driven replay inspection (architecture spec
//! FR6; dx-spec §5–6).
//!
//! A **dry run**: it opens `.keel/journal.db` read-only, walks the recorded
//! step ledger, and renders exactly what re-entering the flow WOULD do —
//! which steps substitute from the journal, which re-execute live, where the
//! replay cursor stands, and what the nondeterminism defense will hold each
//! step to. No effect fires and nothing is written; actual re-execution stays
//! with `keel run` / a front-end resume.
//!
//! The verdict mirrors the core's semantics (`keel-core/src/flow.rs`,
//! normative in `conformance/README.md`):
//!
//! - `completed` → **pure replay**: every step substitutes, no effect fires; a
//!   step beyond the ledger is nondeterminism (KEEL-E031).
//! - `running` / `failed` → **resume**: terminal (`ok`/`error`) records
//!   substitute; a crashed-mid-step `running` record re-executes live; the
//!   flow continues live past the ledger. Each resume consumes one flow-level
//!   attempt, and a still-live lease refuses the resume (KEEL-E030).
//! - `dead` → **refused**: never auto-resumed (KEEL-E032); inspection only.
//!
//! Internal `marker` rows never render as steps, but they are *read*: the
//! reserved seq-0 counter reports how many resumes the flow has consumed, and
//! `flow:branch:*` markers surface as recorded nondeterminism divergences with
//! their expected/observed keys.
//!
//! Determinism (dx-spec §5): the `--json` twin carries only values read from
//! the DB — timestamps are the recorded ms integers, never wall-clock — so
//! identical journals give byte-identical output.

use std::path::Path;

use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::render::to_json;
use crate::{Rendered, evidence, flows};

/// The reserved seq-0 marker key holding the flow-level resume counter
/// (mirrors `keel-core/src/flow.rs::ATTEMPT_KEY`).
const ATTEMPT_KEY: &str = "flow:attempt";

/// The prefix of replay-branch divergence markers journaled by the core's
/// nondeterminism defense (`warn`/`branch` modes).
const BRANCH_PREFIX: &str = "flow:branch:";

/// The schema tag of the core's step-payload envelope
/// (mirrors `keel-core/src/flow.rs::STEP_PAYLOAD_SCHEMA`).
const STEP_PAYLOAD_SCHEMA: &str = "keel.step/v1";

/// What a re-entry would do with one recorded step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    /// The recorded outcome is returned; the effect is not invoked.
    Substitute,
    /// A crashed-mid-step record: the step runs live on resume.
    ReExecute,
    /// A non-terminal record inside a completed flow's pure replay — the
    /// replay would halt with nondeterminism (KEEL-E031) rather than run live.
    ReplayMiss,
    /// A dead flow: no resume happens, so no step does anything.
    None,
}

impl Action {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Substitute => "substitute",
            Self::ReExecute => "re-execute",
            Self::ReplayMiss => "replay-miss",
            Self::None => "none",
        }
    }
}

/// How a re-entry of this flow behaves as a whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Completed flow: every step substitutes; no effect fires.
    PureReplay,
    /// Running/failed flow: substitute the ledger, re-execute the crash point,
    /// continue live.
    Resume,
    /// Dead flow: refused (KEEL-E032).
    Refused,
}

impl Mode {
    fn for_status(status: &str) -> Self {
        match status {
            "completed" => Self::PureReplay,
            "dead" => Self::Refused,
            // 'running' and 'failed' both resume (a failed flow is reset to
            // running before re-leasing).
            _ => Self::Resume,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::PureReplay => "pure-replay",
            Self::Resume => "resume",
            Self::Refused => "refused",
        }
    }
}

/// One raw step row, markers included (they feed the resume counter and the
/// divergence report but never render as steps).
struct StepRow {
    seq: i64,
    step_key: String,
    kind: String,
    attempt: i64,
    outcome: String,
    payload: Option<Vec<u8>>,
    error_class: Option<String>,
    started_at: i64,
    ended_at: Option<i64>,
}

impl StepRow {
    /// Whether this record is terminal (substitutable): `ok` or `error`.
    fn is_terminal(&self) -> bool {
        self.outcome == "ok" || self.outcome == "error"
    }

    /// What a re-entry in `mode` does with this record.
    fn action(&self, mode: Mode) -> Action {
        match mode {
            Mode::Refused => Action::None,
            Mode::PureReplay => {
                if self.is_terminal() {
                    Action::Substitute
                } else {
                    Action::ReplayMiss
                }
            }
            Mode::Resume => {
                if self.is_terminal() {
                    Action::Substitute
                } else {
                    Action::ReExecute
                }
            }
        }
    }
}

/// One rendered step of the replay plan.
#[derive(Debug, Serialize)]
struct PlanStep {
    action: &'static str,
    attempt: i64,
    duration_ms: Option<i64>,
    ended_at: Option<i64>,
    error_class: Option<String>,
    kind: String,
    outcome: String,
    seq: i64,
    started_at: i64,
    step_key: String,
}

/// A nondeterminism divergence the defense already recorded (a
/// `flow:branch:*` marker journaled under `warn`/`branch`).
#[derive(Debug, Serialize)]
struct RecordedDivergence {
    expected: Option<String>,
    mode: Option<String>,
    observed: Option<String>,
    seq: i64,
}

/// The `keel replay <flow>` report — one struct so the human plan and `--json`
/// cannot drift.
#[derive(Debug, Serialize)]
struct ReplayReport {
    /// The recorded deploy fence: a resume under a different `code_hash`
    /// downgrades the nondeterminism response `fail` → `warn` (spec §4.4).
    code_hash: Option<String>,
    created_at: i64,
    divergences: Vec<RecordedDivergence>,
    entrypoint: String,
    flow_id: String,
    lease_expires: Option<i64>,
    lease_holder: Option<String>,
    /// Where live execution would begin on a resume: the first re-executed
    /// seq, or one past the ledger. `null` when nothing runs live
    /// (pure replay, refused).
    live_from_seq: Option<i64>,
    mode: &'static str,
    /// Resumes this flow has consumed so far (the reserved seq-0 counter;
    /// 0 when none is recorded).
    resumes_recorded: i64,
    status: String,
    steps: Vec<PlanStep>,
    steps_reexecute: usize,
    steps_substitute: usize,
    updated_at: i64,
}

/// `keel replay <flow> [--step N]` for `project`. `flow` resolves like
/// `keel trace` (exact `flow_id`, else a unique substring of id/entrypoint).
pub fn replay(project: &Path, flow: &str, step: Option<i64>) -> Rendered {
    let path = evidence::resolved_journal(project).path;
    if !path.exists() {
        return flows::soft_error(
            "no journal yet (.keel/journal.db). Run a flow first with `keel run`.",
        );
    }
    let conn = match flows::open_ro(&path) {
        Ok(c) => c,
        Err(e) => return flows::soft_error(&e),
    };
    let resolved = match flows::resolve_flow(&conn, flow) {
        Ok(r) => r,
        Err(e) => return flows::soft_error(&e),
    };
    let extras = match flow_extras(&conn, &resolved.flow_id) {
        Ok(t) => t,
        Err(e) => return flows::soft_error(&e),
    };
    let rows = match read_step_rows(&conn, &resolved.flow_id) {
        Ok(r) => r,
        Err(e) => return flows::soft_error(&e),
    };

    let mode = Mode::for_status(&resolved.status);
    if let Some(seq) = step {
        return step_detail(&resolved, mode, &rows, seq);
    }

    let report = build_report(&resolved, mode, &rows, extras);
    let human = plan_human(&report);
    Rendered::ok(human, to_json(&report))
}

/// The flow-row columns the trace resolver does not carry.
struct FlowExtras {
    code_hash: Option<String>,
    lease_holder: Option<String>,
    lease_expires: Option<i64>,
}

/// Read `code_hash` and the lease columns for one flow.
fn flow_extras(conn: &Connection, flow_id: &str) -> Result<FlowExtras, String> {
    conn.query_row(
        "SELECT code_hash, lease_holder, lease_expires FROM flows WHERE flow_id = ?1",
        [flow_id],
        |row| {
            Ok(FlowExtras {
                code_hash: row.get(0)?,
                lease_holder: row.get(1)?,
                lease_expires: row.get(2)?,
            })
        },
    )
    .map_err(|e| flows::q(&e))
}

/// Read every step row (markers included) in seq order.
fn read_step_rows(conn: &Connection, flow_id: &str) -> Result<Vec<StepRow>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT seq, step_key, kind, attempt, outcome, payload, error_class, \
             started_at, ended_at FROM steps WHERE flow_id = ?1 ORDER BY seq",
        )
        .map_err(|e| flows::q(&e))?;
    stmt.query_map([flow_id], |row| {
        Ok(StepRow {
            seq: row.get(0)?,
            step_key: row.get(1)?,
            kind: row.get(2)?,
            attempt: row.get(3)?,
            outcome: row.get(4)?,
            payload: row.get(5)?,
            error_class: row.get(6)?,
            started_at: row.get(7)?,
            ended_at: row.get(8)?,
        })
    })
    .map_err(|e| flows::q(&e))?
    .collect::<rusqlite::Result<Vec<_>>>()
    .map_err(|e| flows::q(&e))
}

/// Assemble the full replay plan from the raw rows.
fn build_report(
    resolved: &flows::ResolvedFlow,
    mode: Mode,
    rows: &[StepRow],
    extras: FlowExtras,
) -> ReplayReport {
    let mut steps = Vec::new();
    let mut divergences = Vec::new();
    let mut resumes_recorded = 0;
    for row in rows {
        if row.kind == "marker" {
            if row.step_key == ATTEMPT_KEY {
                resumes_recorded = row.attempt;
            } else if row.step_key.starts_with(BRANCH_PREFIX) {
                divergences.push(divergence_from(row));
            }
            continue;
        }
        steps.push(PlanStep {
            action: row.action(mode).as_str(),
            attempt: row.attempt,
            duration_ms: row.ended_at.map(|e| e - row.started_at),
            ended_at: row.ended_at,
            error_class: row.error_class.clone(),
            kind: row.kind.clone(),
            outcome: row.outcome.clone(),
            seq: row.seq,
            started_at: row.started_at,
            step_key: row.step_key.clone(),
        });
    }
    let steps_substitute = steps
        .iter()
        .filter(|s| s.action == Action::Substitute.as_str())
        .count();
    let steps_reexecute = steps
        .iter()
        .filter(|s| s.action == Action::ReExecute.as_str())
        .count();
    let live_from_seq = match mode {
        Mode::Resume => Some(
            steps
                .iter()
                .find(|s| s.action == Action::ReExecute.as_str())
                .map_or_else(|| steps.last().map_or(1, |s| s.seq + 1), |s| s.seq),
        ),
        Mode::PureReplay | Mode::Refused => None,
    };
    ReplayReport {
        code_hash: extras.code_hash,
        created_at: resolved.created_at,
        divergences,
        entrypoint: resolved.entrypoint.clone(),
        flow_id: resolved.flow_id.clone(),
        lease_expires: extras.lease_expires,
        lease_holder: extras.lease_holder,
        live_from_seq,
        mode: mode.as_str(),
        resumes_recorded,
        status: resolved.status.clone(),
        steps,
        steps_reexecute,
        steps_substitute,
        updated_at: resolved.updated_at,
    }
}

/// Decode a `flow:branch:*` marker into its expected/observed keys.
fn divergence_from(row: &StepRow) -> RecordedDivergence {
    let payload = row.payload.as_deref().and_then(decode_payload);
    let field = |name: &str| -> Option<String> {
        payload
            .as_ref()
            .and_then(|p| p.get(name))
            .and_then(|v| v.as_str())
            .map(str::to_owned)
    };
    RecordedDivergence {
        expected: field("expected"),
        mode: field("mode"),
        observed: field("observed"),
        seq: row.seq,
    }
}

/// The human replay plan, derived entirely from [`ReplayReport`].
fn plan_human(report: &ReplayReport) -> String {
    let mut lines = vec![
        format!(
            "keel \u{25b8} replay {} (dry run \u{2014} no effect fires)\n",
            report.flow_id
        ),
        format!("  entrypoint: {}\n", report.entrypoint),
        format!("  status:     {}\n", report.status),
        format!("  verdict:    {}\n", verdict_line(report)),
    ];
    // The deploy fence and the lease only gate a live resume; a pure replay
    // substitutes regardless and a dead flow is refused before either check.
    if report.mode == Mode::Resume.as_str() {
        if let Some(hash) = &report.code_hash {
            lines.push(format!(
                "  code fence: {hash} \u{2014} a resume under a different deploy downgrades the \
                 nondeterminism response fail \u{2192} warn\n"
            ));
        }
        if let (Some(holder), Some(expires)) = (&report.lease_holder, report.lease_expires) {
            lines.push(format!(
                "  lease:      {holder} (expires at {expires}) \u{2014} a resume while this \
                 lease is live is refused (KEEL-E030)\n"
            ));
        }
    }
    if report.resumes_recorded > 0 {
        lines.push(format!(
            "  resumes:    {} consumed so far (flow-level attempt counter)\n",
            report.resumes_recorded
        ));
    }
    lines.push(format!("  steps:      {}\n", report.steps.len()));
    for s in &report.steps {
        lines.push(format!(
            "    {:>3}. {:<7} {:<28} {:<8} \u{2192} {}\n",
            s.seq, s.kind, s.step_key, s.outcome, s.action,
        ));
    }
    if !report.divergences.is_empty() {
        lines.push("  recorded divergences (nondeterminism defense, KEEL-E031):\n".to_owned());
        for d in &report.divergences {
            lines.push(format!(
                "    seq {}: {} \u{2014} expected {}, observed {}\n",
                d.seq,
                d.mode.as_deref().unwrap_or("?"),
                d.expected.as_deref().unwrap_or("?"),
                d.observed.as_deref().unwrap_or("?"),
            ));
        }
    }
    lines.push(cursor_line(report));
    lines.concat()
}

/// One sentence: what a re-entry of this flow does.
fn verdict_line(report: &ReplayReport) -> String {
    match report.mode {
        "pure-replay" => format!(
            "pure replay \u{2014} all {} steps substitute from the journal; no effect fires",
            report.steps_substitute
        ),
        "refused" => "refused \u{2014} dead flows are never auto-resumed (KEEL-E032)".to_owned(),
        _ => format!(
            "resume \u{2014} {} of {} steps substitute; {} re-execute{} live",
            report.steps_substitute,
            report.steps.len(),
            report.steps_reexecute,
            if report.steps_reexecute == 1 { "s" } else { "" },
        ),
    }
}

/// The closing "where the cursor stands / what to do next" line.
fn cursor_line(report: &ReplayReport) -> String {
    match report.mode {
        "pure-replay" => {
            let end = report.steps.last().map_or(0, |s| s.seq);
            format!(
                "  cursor:     end of ledger \u{2014} the result reconstructs entirely from the \
                 journal; a step beyond seq {end} would be nondeterminism (KEEL-E031)\n"
            )
        }
        "refused" => {
            "  next:       inspect the poison step with `keel trace`, fix the cause, and rerun \
             with a new flow identity; see `keel explain KEEL-E032`\n"
                .to_owned()
        }
        _ => {
            let seq = report.live_from_seq.unwrap_or(1);
            format!(
                "  cursor:     live execution resumes at seq {seq}; steps beyond the ledger run \
                 live and are journaled\n"
            )
        }
    }
}

/// The `--step N` detail report: one recorded step in full.
#[derive(Debug, Serialize)]
struct StepDetail {
    action: &'static str,
    attempt: i64,
    duration_ms: Option<i64>,
    ended_at: Option<i64>,
    error_class: Option<String>,
    flow_id: String,
    kind: String,
    mode: &'static str,
    outcome: String,
    /// The decoded MessagePack payload (`null` when absent or undecodable).
    payload: Option<serde_json::Value>,
    payload_bytes: Option<usize>,
    seq: i64,
    started_at: i64,
    status: String,
    step_key: String,
}

/// Render the `--step N` view, or a precise error when `seq` is not recorded.
fn step_detail(resolved: &flows::ResolvedFlow, mode: Mode, rows: &[StepRow], seq: i64) -> Rendered {
    let Some(row) = rows.iter().find(|r| r.seq == seq) else {
        let real: Vec<i64> = rows
            .iter()
            .filter(|r| r.kind != "marker")
            .map(|r| r.seq)
            .collect();
        let range = match (real.first(), real.last()) {
            (Some(lo), Some(hi)) => format!("recorded steps: seq {lo}\u{2013}{hi}"),
            _ => "no steps recorded".to_owned(),
        };
        return flows::soft_error(&format!(
            "flow {} has no step at seq {seq} ({range}). Run `keel replay {}` to see the ledger.",
            resolved.flow_id, resolved.flow_id
        ));
    };
    // Markers never replay; render them with action "none" for inspection.
    let action = if row.kind == "marker" {
        Action::None
    } else {
        row.action(mode)
    };
    let payload = row.payload.as_deref().and_then(decode_payload);
    let report = StepDetail {
        action: action.as_str(),
        attempt: row.attempt,
        duration_ms: row.ended_at.map(|e| e - row.started_at),
        ended_at: row.ended_at,
        error_class: row.error_class.clone(),
        flow_id: resolved.flow_id.clone(),
        kind: row.kind.clone(),
        mode: mode.as_str(),
        outcome: row.outcome.clone(),
        payload,
        payload_bytes: row.payload.as_ref().map(Vec::len),
        seq: row.seq,
        started_at: row.started_at,
        status: resolved.status.clone(),
        step_key: row.step_key.clone(),
    };
    let human = step_human(&report);
    Rendered::ok(human, to_json(&report))
}

/// The human `--step` view, derived entirely from [`StepDetail`].
fn step_human(report: &StepDetail) -> String {
    let recorded = match report.ended_at {
        Some(end) => format!(
            "{} \u{2192} {end} ({}ms)",
            report.started_at,
            report.duration_ms.unwrap_or(0)
        ),
        None => format!("{} \u{2192} \u{2014} (still running)", report.started_at),
    };
    let payload = match (&report.payload, report.payload_bytes) {
        (Some(v), _) => v.to_string(),
        (None, Some(n)) => format!("({n} bytes, not decodable as MessagePack)"),
        (None, None) => "\u{2014}".to_owned(),
    };
    let mut lines = vec![
        format!(
            "keel \u{25b8} replay {} \u{2014} step {}\n",
            report.flow_id, report.seq
        ),
        format!("  step_key:  {}\n", report.step_key),
        format!("  kind:      {}\n", report.kind),
        format!(
            "  outcome:   {} (attempt {})\n",
            report.outcome, report.attempt
        ),
        format!("  action:    {}\n", action_sentence(report.action)),
        format!("  recorded:  {recorded}\n"),
        format!("  payload:   {payload}\n"),
    ];
    if let Some(class) = &report.error_class {
        lines.push(format!("  error:     class {class}\n"));
    }
    lines.concat()
}

/// Expand an action token into its one-line meaning.
fn action_sentence(action: &str) -> String {
    let meaning = match action {
        "substitute" => "the recorded outcome is returned; the effect is not invoked",
        "re-execute" => "crashed mid-step; a resume runs this step live",
        "replay-miss" => "not terminal inside a completed flow; replay halts (KEEL-E031)",
        _ => "internal marker or refused flow; nothing runs",
    };
    format!("{action} \u{2014} {meaning}")
}

/// Decode a step payload: the core's schema-tagged envelope
/// (`{schema: "keel.step/v1", payload}`), falling back to a bare value so
/// pre-tag journals — including the golden fixtures — still render.
fn decode_payload(bytes: &[u8]) -> Option<serde_json::Value> {
    #[derive(Deserialize)]
    struct Envelope {
        schema: String,
        payload: serde_json::Value,
    }
    if let Ok(envelope) = rmp_serde::from_slice::<Envelope>(bytes)
        && envelope.schema == STEP_PAYLOAD_SCHEMA
    {
        return Some(envelope.payload);
    }
    rmp_serde::from_slice(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EXIT_FAILURE, EXIT_OK};
    use std::path::PathBuf;

    /// Build a project dir with `.keel/journal.db` from a golden fixture
    /// (`conformance/fixtures/journal/<fixture>`), applied over the frozen
    /// schema — same shape as the `flows` tests.
    fn project_with_fixture(fixture: &str) -> (tempfile::TempDir, PathBuf) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let schema = std::fs::read_to_string(root.join("contracts/journal.sql")).unwrap();
        let sql = std::fs::read_to_string(root.join("conformance/fixtures/journal").join(fixture))
            .unwrap();
        let dir = tempfile::TempDir::new().unwrap();
        let keel = dir.path().join(".keel");
        std::fs::create_dir_all(&keel).unwrap();
        let conn = Connection::open(keel.join("journal.db")).unwrap();
        conn.execute_batch(&schema).unwrap();
        conn.execute_batch(&sql).unwrap();
        let project = dir.path().to_path_buf();
        (dir, project)
    }

    /// Open the fixture journal read-write for test-only extra inserts.
    fn open_rw(project: &Path) -> Connection {
        Connection::open(project.join(".keel/journal.db")).unwrap()
    }

    #[test]
    fn completed_flow_is_pure_replay_all_substitute() {
        let (_d, project) = project_with_fixture("completed-flow.sql");
        let r = replay(&project, "01JZWY0A0000000000000001", None);
        assert_eq!(r.exit, EXIT_OK);
        assert_eq!(r.json["mode"], "pure-replay");
        assert_eq!(r.json["steps_substitute"], 5);
        assert_eq!(r.json["steps_reexecute"], 0);
        assert!(r.json["live_from_seq"].is_null());
        assert_eq!(r.json["code_hash"], "ch-9b2e44");
        for s in r.json["steps"].as_array().unwrap() {
            assert_eq!(s["action"], "substitute");
        }
        assert!(r.human.contains("dry run"));
        assert!(r.human.contains("pure replay"));
        assert!(r.human.contains("KEEL-E031"));
    }

    #[test]
    fn interrupted_flow_resumes_at_the_crashed_step() {
        let (_d, project) = project_with_fixture("interrupted-flow.sql");
        let r = replay(&project, "01JZWY0A0000000000000002", None);
        assert_eq!(r.exit, EXIT_OK);
        assert_eq!(r.json["mode"], "resume");
        assert_eq!(r.json["steps_substitute"], 3);
        assert_eq!(r.json["steps_reexecute"], 1);
        assert_eq!(r.json["live_from_seq"], 4);
        // The crashed step is the one that re-executes.
        let steps = r.json["steps"].as_array().unwrap();
        assert_eq!(steps[3]["seq"], 4);
        assert_eq!(steps[3]["action"], "re-execute");
        assert_eq!(steps[3]["outcome"], "running");
        // The lease columns surface verbatim (a live lease refuses a resume).
        assert_eq!(r.json["lease_holder"], "host-a:pid-4242");
        assert_eq!(r.json["lease_expires"], 1_783_728_030_000_i64);
        assert!(r.human.contains("live execution resumes at seq 4"));
        assert!(r.human.contains("KEEL-E030"));
    }

    #[test]
    fn dead_flow_is_refused_inspection_only() {
        let (_d, project) = project_with_fixture("dead-flow.sql");
        let r = replay(&project, "01JZWY0A0000000000000003", None);
        assert_eq!(r.exit, EXIT_OK, "inspection of a dead flow still succeeds");
        assert_eq!(r.json["mode"], "refused");
        assert_eq!(r.json["steps_substitute"], 0);
        assert!(r.json["live_from_seq"].is_null());
        for s in r.json["steps"].as_array().unwrap() {
            assert_eq!(s["action"], "none");
        }
        assert!(r.human.contains("KEEL-E032"));
        assert!(r.human.contains("keel explain KEEL-E032"));
    }

    #[test]
    fn resume_past_a_complete_ledger_continues_live_after_it() {
        // A running flow whose recorded steps are all terminal (crash landed
        // between steps): everything substitutes, live from last seq + 1.
        let (_d, project) = project_with_fixture("interrupted-flow.sql");
        open_rw(&project)
            .execute(
                "UPDATE steps SET outcome = 'ok', ended_at = started_at + 10 \
                 WHERE flow_id = '01JZWY0A0000000000000002' AND seq = 4",
                [],
            )
            .unwrap();
        let r = replay(&project, "01JZWY0A0000000000000002", None);
        assert_eq!(r.json["steps_substitute"], 4);
        assert_eq!(r.json["steps_reexecute"], 0);
        assert_eq!(r.json["live_from_seq"], 5);
    }

    #[test]
    fn attempt_marker_and_branch_marker_surface_without_becoming_steps() {
        let (_d, project) = project_with_fixture("interrupted-flow.sql");
        let conn = open_rw(&project);
        // The reserved seq-0 resume counter: 2 resumes consumed.
        conn.execute(
            "INSERT INTO steps VALUES ('01JZWY0A0000000000000002', 0, 'flow:attempt', \
             'marker', 2, 'ok', NULL, NULL, 1783728000000, 1783728000000)",
            [],
        )
        .unwrap();
        // A recorded warn-mode divergence marker with the core's envelope.
        let payload = rmp_serde::to_vec_named(&serde_json::json!({
            "schema": "keel.step/v1",
            "payload": {"mode": "warn", "expected": "a#1", "observed": "b#1"},
        }))
        .unwrap();
        conn.execute(
            "INSERT INTO steps VALUES ('01JZWY0A0000000000000002', 5, 'flow:branch:warn', \
             'marker', 0, 'ok', ?1, NULL, 1783728002000, 1783728002000)",
            [payload],
        )
        .unwrap();
        let r = replay(&project, "01JZWY0A0000000000000002", None);
        assert_eq!(r.json["resumes_recorded"], 2);
        // Markers are not steps: still the 4 real ones.
        assert_eq!(r.json["steps"].as_array().unwrap().len(), 4);
        let d = &r.json["divergences"][0];
        assert_eq!(d["seq"], 5);
        assert_eq!(d["mode"], "warn");
        assert_eq!(d["expected"], "a#1");
        assert_eq!(d["observed"], "b#1");
        assert!(r.human.contains("recorded divergences"));
        assert!(r.human.contains("expected a#1, observed b#1"));
        assert!(r.human.contains("resumes:    2 consumed"));
    }

    #[test]
    fn step_detail_decodes_the_recorded_payload() {
        let (_d, project) = project_with_fixture("completed-flow.sql");
        let r = replay(&project, "01JZWY0A0000000000000001", Some(1));
        assert_eq!(r.exit, EXIT_OK);
        assert_eq!(r.json["step_key"], "api.source.internal#q1");
        assert_eq!(r.json["action"], "substitute");
        assert_eq!(r.json["payload"]["rows"], 120);
        assert_eq!(r.json["duration_ms"], 240);
        assert!(r.human.contains("{\"rows\":120}"));
    }

    #[test]
    fn step_detail_shows_error_class_and_null_payload() {
        let (_d, project) = project_with_fixture("dead-flow.sql");
        let r = replay(&project, "01JZWY0A0000000000000003", Some(2));
        assert_eq!(r.json["outcome"], "error");
        assert_eq!(r.json["error_class"], "http");
        assert!(r.json["payload"].is_null());
        assert!(r.json["payload_bytes"].is_null());
        assert_eq!(r.json["attempt"], 5);
        assert!(r.human.contains("class http"));
    }

    #[test]
    fn step_detail_unknown_seq_says_what_exists() {
        let (_d, project) = project_with_fixture("completed-flow.sql");
        let r = replay(&project, "01JZWY0A0000000000000001", Some(9));
        assert_eq!(r.exit, EXIT_FAILURE);
        assert!(r.to_stderr);
        assert!(r.human.contains("no step at seq 9"));
        assert!(r.human.contains("seq 1\u{2013}5"));
        assert!(r.human.contains("keel replay"));
    }

    #[test]
    fn unknown_flow_is_a_soft_error_with_a_next_step() {
        let (_d, project) = project_with_fixture("completed-flow.sql");
        let r = replay(&project, "does-not-exist", None);
        assert_eq!(r.exit, EXIT_FAILURE);
        assert!(r.to_stderr);
        assert!(r.human.contains("no flow matches"));
        assert!(r.human.contains("keel flows"));
    }

    #[test]
    fn absent_journal_nudges_toward_keel_run() {
        let dir = tempfile::TempDir::new().unwrap();
        let r = replay(dir.path(), "anything", None);
        assert_eq!(r.exit, EXIT_FAILURE);
        assert!(r.human.contains("no journal yet"));
    }
}
