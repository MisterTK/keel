//! `keel flows force <FLOW>` — arm the durable one-shot KEEL-E033 force
//! override (CCR-5 decision 2 / CCR-6) for one flow.
//!
//! `keel exec --force` is an in-memory, per-invocation flag: it only helps an
//! operator who is *at* the terminal re-running the command themselves. In-
//! process `cmd:` interception (issue #27) and stuck flows whose host already
//! exited have no such surface — the next re-dispatch happens somewhere the
//! `--force` flag can't reach. This verb is the out-of-process, config-free
//! escape hatch: it persists a marker step (via
//! [`exec::request_force_override`]) so the next re-dispatch that would
//! otherwise be KEEL-E033-refused (its declared side-effect files changed since
//! the last attempt) proceeds exactly once, then clears itself.
//!
//! Only a `running`/`failed` flow is ever gated by KEEL-E033
//! (`exec::side_effect_gate` checks no other status), so forcing a `completed`
//! or `dead` flow would arm a no-op; those are refused with a precise
//! what/why/next instead, mirroring [`crate::resume`]'s eligibility UX.

use std::path::Path;

use keel_journal::{FlowId, SqliteJournal, SystemClock};
use serde::Serialize;

use crate::render::to_json;
use crate::{EXIT_OK, Rendered, evidence, exec, flows};

/// The `keel flows force` `--json` twin.
#[derive(Debug, Serialize)]
struct ForceReport {
    /// Always `true` on the success path (a failure renders through
    /// [`flows::soft_error`] with an `error` field instead).
    armed: bool,
    /// The exact flow_id the one-shot was armed for (resolved from the
    /// possibly-partial `<FLOW>` argument).
    flow_id: String,
}

/// `keel flows force <FLOW>` for `project`: resolve `<FLOW>` to exactly one
/// flow and arm its durable one-shot KEEL-E033 override.
///
/// Returns the rendered result and the process exit code: [`EXIT_OK`] on a
/// successful arm, otherwise a soft error (exit 1) — an unresolvable/ambiguous
/// flow, a `completed`/`dead` flow (nothing to force), or an unwritable
/// journal. Never panics; no raw SQL error ever reaches the user.
pub fn run(project: &Path, flow: &str) -> (Option<Rendered>, i32) {
    let journal_path = evidence::resolved_journal(project).path;
    if !journal_path.exists() {
        return soft_pair(
            "no journal yet (.keel/journal.db). Run a flow first with `keel run` or `keel exec`.",
        );
    }

    // Resolve `<FLOW>` against a throwaway READ-ONLY connection (reusing
    // `resolve_flow`'s exact-id-or-unique-substring logic verbatim), then drop
    // it before opening the journal read-write. One writer at a time (issue
    // #14); resolution itself never needs to write.
    let resolved = {
        let conn = match flows::open_ro(&journal_path) {
            Ok(c) => c,
            Err(e) => return soft_pair(&e),
        };
        match flows::resolve_flow(&conn, flow) {
            Ok(r) => r,
            Err(e) => return soft_pair(&e),
        }
    };

    // Refuse the two statuses KEEL-E033 never gates: arming them would silently
    // do nothing. A precise message beats a no-op that looks like success.
    match resolved.status.as_str() {
        "completed" => {
            return soft_pair(&format!(
                "flow {id} is already completed; there is nothing to force \u{2014} a completed \
                 flow's command never re-runs (KEEL-E033 only gates a running/failed \
                 re-dispatch). Inspect it with `keel replay {id}` or `keel trace {id}`.",
                id = resolved.flow_id
            ));
        }
        "dead" => {
            return soft_pair(&format!(
                "flow {id} is dead (KEEL-E032); forcing the KEEL-E033 side-effect gate cannot \
                 revive it \u{2014} a dead flow is never re-dispatched. See `keel explain \
                 KEEL-E032`, then start a new flow with a fresh identity instead.",
                id = resolved.flow_id
            ));
        }
        _ => {}
    }

    // Open the journal READ-WRITE and arm the one-shot. The journal file and
    // the flow row both already exist (guarded above / proven by resolution),
    // so `SqliteJournal::open` materializes nothing spurious — the same open
    // `keel exec` itself uses. All access is through this ONE handle (issue
    // #14: no second in-process reader against a live journal).
    let journal = match SqliteJournal::open(&journal_path, SystemClock) {
        Ok(j) => j,
        Err(e) => {
            return soft_pair(&format!(
                "could not open the journal at {} read-write: {e}",
                journal_path.display()
            ));
        }
    };
    let flow_id = FlowId::new(resolved.flow_id.as_str());
    if let Err(e) = exec::request_force_override(&journal, &flow_id) {
        return soft_pair(&format!(
            "could not arm the force override for flow {}: {e}",
            resolved.flow_id
        ));
    }

    let human = format!(
        "keel \u{25b8} flows force: {} armed \u{2014} the next re-dispatch that would otherwise be \
         KEEL-E033-refused (its declared side-effect files changed) will proceed once, then this \
         one-shot clears itself.",
        resolved.flow_id
    );
    let report = ForceReport {
        armed: true,
        flow_id: resolved.flow_id,
    };
    (Some(Rendered::ok(human, to_json(&report))), EXIT_OK)
}

/// A soft error (exit 1, on stderr) — mirrors `keel exec`/`keel flows resume`.
fn soft_pair(message: &str) -> (Option<Rendered>, i32) {
    let r = flows::soft_error(message);
    let code = r.exit;
    (Some(r), code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::{Connection, params};

    const T0: i64 = 1_783_728_000_000;

    /// A project dir with `.keel/journal.db` built from the frozen schema plus
    /// the named golden fixtures (the exact helper `resume`'s tests use).
    fn project_with_fixtures(fixtures: &[&str]) -> (tempfile::TempDir, std::path::PathBuf) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let dir = tempfile::TempDir::new().unwrap();
        let keel = dir.path().join(".keel");
        std::fs::create_dir_all(&keel).unwrap();
        let conn = Connection::open(keel.join("journal.db")).unwrap();
        let schema = std::fs::read_to_string(root.join("contracts/journal.sql")).unwrap();
        conn.execute_batch(&schema).unwrap();
        for f in fixtures {
            let sql =
                std::fs::read_to_string(root.join("conformance/fixtures/journal").join(f)).unwrap();
            conn.execute_batch(&sql).unwrap();
        }
        let project = dir.path().to_path_buf();
        (dir, project)
    }

    /// Seed one `failed` `cmd:` flow row directly (the status `keel flows
    /// force` actually arms) and return its id.
    fn seed_failed_cmd_flow(project: &Path, flow_id: &str) {
        let conn = Connection::open(project.join(".keel/journal.db")).unwrap();
        conn.execute(
            "INSERT INTO flows (flow_id, entrypoint, args_hash, status, created_at, updated_at) \
             VALUES (?1, 'cmd:trade', 'ah', 'failed', ?2, ?2)",
            params![flow_id, T0],
        )
        .unwrap();
    }

    #[test]
    fn missing_journal_is_a_soft_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let (r, code) = run(dir.path(), "anything");
        assert_eq!(code, crate::EXIT_FAILURE);
        assert!(r.unwrap().human.contains("no journal yet"));
    }

    #[test]
    fn unknown_flow_is_a_clean_soft_error_not_a_panic() {
        let (_d, project) = project_with_fixtures(&["completed-flow.sql"]);
        let (r, code) = run(&project, "does-not-exist");
        assert_eq!(code, crate::EXIT_FAILURE);
        assert!(r.unwrap().human.contains("no flow matches"));
    }

    #[test]
    fn ambiguous_flow_is_a_soft_error() {
        let (_d, project) = project_with_fixtures(&["completed-flow.sql", "dead-flow.sql"]);
        // Both fixture ids share this prefix, so resolution refuses to guess.
        let (r, code) = run(&project, "01JZWY0A");
        assert_eq!(code, crate::EXIT_FAILURE);
        assert!(r.unwrap().human.contains("matches"));
    }

    #[test]
    fn completed_flow_is_refused_as_nothing_to_force() {
        let (_d, project) = project_with_fixtures(&["completed-flow.sql"]);
        let (r, code) = run(&project, "01JZWY0A0000000000000001");
        assert_eq!(code, crate::EXIT_FAILURE);
        let human = r.unwrap().human;
        assert!(human.contains("already completed"));
        assert!(human.contains("nothing to force"));
    }

    #[test]
    fn dead_flow_is_refused_with_e032() {
        let (_d, project) = project_with_fixtures(&["dead-flow.sql"]);
        let (r, code) = run(&project, "01JZWY0A0000000000000003");
        assert_eq!(code, crate::EXIT_FAILURE);
        let human = r.unwrap().human;
        assert!(human.contains("KEEL-E032"));
        assert!(human.contains("dead"));
    }

    #[test]
    fn arming_a_failed_flow_records_the_requested_force_marker() {
        let (_d, project) = project_with_fixtures(&[]);
        seed_failed_cmd_flow(&project, "01FORCEME");

        let (r, code) = run(&project, "01FORCEME");
        assert_eq!(code, EXIT_OK);
        let rendered = r.unwrap();
        assert!(rendered.human.contains("01FORCEME"));
        assert!(rendered.human.contains("armed"));
        assert_eq!(rendered.json["armed"], true);
        assert_eq!(rendered.json["flow_id"], "01FORCEME");

        // The one-shot marker landed at the reserved FORCE_SEQ as a `marker`
        // row with the reserved `cmd:force` key — the exact row the gate
        // reads-and-clears. (The payload state=`requested` semantics are
        // proven end-to-end through the real gate in `tests/flows_force.rs`
        // and unit-level in `exec.rs`'s force-override test; the payload here
        // is a MessagePack blob, not a queryable string.)
        let conn = Connection::open(project.join(".keel/journal.db")).unwrap();
        let (step_key, kind, outcome, payload_len): (String, String, String, i64) = conn
            .query_row(
                "SELECT step_key, kind, outcome, length(payload) FROM steps \
                 WHERE flow_id = '01FORCEME' AND seq = 500002",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("the FORCE marker row exists");
        assert_eq!(step_key, "cmd:force");
        assert_eq!(kind, "marker");
        assert_eq!(outcome, "ok");
        assert!(payload_len > 0, "the armed marker carries a payload");
    }

    #[test]
    fn substring_resolution_arms_the_uniquely_matching_flow() {
        let (_d, project) = project_with_fixtures(&[]);
        seed_failed_cmd_flow(&project, "01UNIQUEABCDEF");
        // A unique substring of the id resolves the same way an exact id does.
        let (r, code) = run(&project, "UNIQUEABC");
        assert_eq!(code, EXIT_OK);
        assert_eq!(r.unwrap().json["flow_id"], "01UNIQUEABCDEF");
    }
}
