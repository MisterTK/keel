//! Journal maintenance: the checks and safe repairs behind `keel fsck`
//! (architecture spec §6 — "journal corruption → SQLite WAL recovery, plus
//! `keel fsck`") and its retention pruning.
//!
//! This module is deliberately *not* part of the [`Journal`](crate::Journal)
//! trait: maintenance is an offline administrative concern, while the trait is
//! the engine's hot persistence boundary. [`JournalAdmin`] opens the SQLite
//! file directly — read-only for a check pass, read-write (never creating) for
//! repairs — and exposes one method per invariant.
//!
//! ## The invariants checked
//!
//! - **SQLite integrity** (`PRAGMA integrity_check`) — page-level corruption a
//!   crash or foreign tool could leave. Not repairable here; WAL recovery
//!   already ran at open, so a failure means restore-or-recreate.
//! - **Orphan steps** — `steps` rows whose `flow_id` has no `flows` row. The
//!   schema's foreign key forbids them, but `PRAGMA foreign_keys` is
//!   per-connection, so a foreign writer can create them. They are dangerous,
//!   not just untidy: flow ids are deterministic (`entrypoint#args_hash#key`),
//!   so a rerun would `begin_flow` the same id and *replay a stranger's
//!   steps*. Repair: delete.
//! - **Dangling leases** — a `lease_holder`/`lease_expires` on a flow that is
//!   no longer `running`. `complete_flow` always clears the lease, so a
//!   dangling one is inconsistent leftovers; it is inert (leases are only
//!   consulted for `running` flows) but misleading to inspection. Repair:
//!   clear, without touching `updated_at` (fsck repairs must not change what
//!   `keel flows` ages or retention measures).
//! - **Stale running steps** — a step still marked `running` inside a flow in
//!   a *no-further-execution* status (`completed`/`dead`). A `running` step in
//!   a `running`/`failed` flow is legitimate crash evidence that resume
//!   re-executes; in a completed/dead flow nothing will ever finish it.
//!   Repair: delete (replay substitutes only terminal `ok`/`error` outcomes,
//!   so a `running` row is never substituted — removing it changes nothing).
//! - **Expired cache rows** — `cache` rows past `expires_at`. The read path
//!   already filters them, so they are invisible garbage that grows the file
//!   without bound. Repair: delete.
//! - **Dead flows** — reported, never repaired or pruned: a dead flow is the
//!   evidence of a poison failure (`keel flows --dead`, `keel trace`).
//!
//! ## Retention ([`prune_completed_flows`](JournalAdmin::prune_completed_flows))
//!
//! Deletes `completed` flows (and their steps) whose `updated_at` is older
//! than a cutoff. Conservative by design: only terminal-success flows are
//! eligible — never `running` (resumable), `failed` (resumable), or `dead`
//! (evidence) — and a flow with any `outbox` rows is kept (the outbox is a
//! reliable-handoff ledger; retention must not drop it). **Semantic caveat**:
//! a pruned flow's journal is gone, so a rerun with the same identity starts
//! a *fresh* flow and re-executes live instead of replaying — pruning trades
//! replayability of old successes for a bounded file.

use std::path::Path;

use rusqlite::{Connection, OpenFlags, params};

use crate::error::Result;

/// The outcome of `PRAGMA integrity_check`: either a clean bill, or the
/// messages SQLite reported (including "file is not a database" for a file
/// that is not SQLite at all — folded here because the *finding* is the data).
#[derive(Debug, Clone)]
pub struct IntegrityOutcome {
    /// Whether the database passed the check.
    pub ok: bool,
    /// SQLite's findings when it did not (empty when `ok`).
    pub detail: Vec<String>,
}

/// One step still marked `running` inside a completed/dead flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleRunningStep {
    /// The flow the step belongs to.
    pub flow_id: String,
    /// The step's sequence number within the flow.
    pub seq: i64,
}

/// What a retention prune removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PruneOutcome {
    /// Completed flows deleted.
    pub flows_deleted: u64,
    /// Their step rows deleted.
    pub steps_deleted: u64,
}

/// Predicate naming the flows a prune may touch: `completed`, older than the
/// cutoff, and without outbox rows (see module docs). One definition shared by
/// the preview and the delete so they can never disagree.
const PRUNABLE: &str = "status = 'completed' AND updated_at < ?1 \
     AND flow_id NOT IN (SELECT flow_id FROM outbox WHERE flow_id IS NOT NULL)";

/// A maintenance handle on one SQLite journal file. See the module docs for
/// the invariants; construction never creates or initializes a file — fsck
/// inspects what exists.
pub struct JournalAdmin {
    conn: Connection,
}

impl core::fmt::Debug for JournalAdmin {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("JournalAdmin").finish_non_exhaustive()
    }
}

impl JournalAdmin {
    /// Open `path` read-only (a check pass; never takes a write lock, so it is
    /// safe against a live application and on read-only mounts).
    pub fn open_readonly(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        conn.execute_batch("PRAGMA busy_timeout = 5000;")?;
        Ok(Self { conn })
    }

    /// Open `path` read-write for repairs/pruning — without `CREATE`: fsck
    /// repairs journals, it never conjures one.
    pub fn open_readwrite(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        conn.execute_batch("PRAGMA busy_timeout = 5000;")?;
        Ok(Self { conn })
    }

    /// `PRAGMA integrity_check`, with any error (e.g. "file is not a
    /// database") folded into the outcome — for fsck the failure *is* the
    /// finding, not an abort.
    #[must_use]
    pub fn integrity_check(&self) -> IntegrityOutcome {
        let rows: rusqlite::Result<Vec<String>> = self
            .conn
            .prepare("PRAGMA integrity_check")
            .and_then(|mut stmt| {
                stmt.query_map([], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()
            });
        match rows {
            Ok(messages) if messages == ["ok"] => IntegrityOutcome {
                ok: true,
                detail: Vec::new(),
            },
            Ok(messages) => IntegrityOutcome {
                ok: false,
                detail: messages,
            },
            Err(e) => IntegrityOutcome {
                ok: false,
                detail: vec![e.to_string()],
            },
        }
    }

    /// Whether the frozen journal schema's tables are present — distinguishes
    /// "a keel journal" from "some other SQLite file at this path".
    pub fn schema_present(&self) -> Result<bool> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' \
             AND name IN ('flows', 'steps', 'cache')",
            [],
            |row| row.get(0),
        )?;
        Ok(count == 3)
    }

    /// Cache rows already past `expires_at` (invisible to reads, still on disk).
    pub fn expired_cache_count(&self, now_ms: i64) -> Result<u64> {
        self.count("SELECT COUNT(*) FROM cache WHERE expires_at <= ?1", now_ms)
    }

    /// Delete expired cache rows; returns how many were swept. Safe: the read
    /// path already treats them as absent.
    pub fn sweep_expired_cache(&self, now_ms: i64) -> Result<u64> {
        let n = self
            .conn
            .execute("DELETE FROM cache WHERE expires_at <= ?1", params![now_ms])?;
        Ok(n as u64)
    }

    /// Step rows whose flow does not exist (see module docs for why these are
    /// dangerous, not just untidy).
    pub fn orphan_step_count(&self) -> Result<u64> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM steps WHERE flow_id NOT IN (SELECT flow_id FROM flows)",
            [],
            |row| row.get(0),
        )?;
        Ok(u64::try_from(n).unwrap_or(0))
    }

    /// The distinct flow ids orphan steps point at, sorted (deterministic).
    pub fn orphan_step_flow_ids(&self) -> Result<Vec<String>> {
        self.ids(
            "SELECT DISTINCT flow_id FROM steps \
             WHERE flow_id NOT IN (SELECT flow_id FROM flows) ORDER BY flow_id",
        )
    }

    /// Delete orphan steps; returns rows removed.
    pub fn delete_orphan_steps(&self) -> Result<u64> {
        let n = self.conn.execute(
            "DELETE FROM steps WHERE flow_id NOT IN (SELECT flow_id FROM flows)",
            [],
        )?;
        Ok(n as u64)
    }

    /// Flows in a non-`running` status still carrying lease fields, sorted.
    pub fn dangling_lease_flow_ids(&self) -> Result<Vec<String>> {
        self.ids(
            "SELECT flow_id FROM flows WHERE status != 'running' \
             AND (lease_holder IS NOT NULL OR lease_expires IS NOT NULL) ORDER BY flow_id",
        )
    }

    /// Clear dangling leases; returns flows repaired. `updated_at` is left
    /// untouched — a repair must not change what `keel flows` ages or what
    /// retention measures.
    pub fn clear_dangling_leases(&self) -> Result<u64> {
        let n = self.conn.execute(
            "UPDATE flows SET lease_holder = NULL, lease_expires = NULL \
             WHERE status != 'running' \
             AND (lease_holder IS NOT NULL OR lease_expires IS NOT NULL)",
            [],
        )?;
        Ok(n as u64)
    }

    /// Steps still `running` inside a `completed`/`dead` flow, ordered by
    /// `(flow_id, seq)`. A `running` step in a `running`/`failed` flow is
    /// legitimate crash evidence and is *not* reported.
    pub fn stale_running_steps(&self) -> Result<Vec<StaleRunningStep>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.flow_id, s.seq FROM steps s \
             JOIN flows f ON f.flow_id = s.flow_id \
             WHERE s.outcome = 'running' AND f.status IN ('completed', 'dead') \
             ORDER BY s.flow_id, s.seq",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(StaleRunningStep {
                    flow_id: row.get(0)?,
                    seq: row.get(1)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Delete stale running steps; returns rows removed. Safe: replay
    /// substitutes only terminal (`ok`/`error`) outcomes, so a `running` row
    /// is never substituted and removing it changes no observable behavior.
    pub fn sweep_stale_running_steps(&self) -> Result<u64> {
        let n = self.conn.execute(
            "DELETE FROM steps WHERE outcome = 'running' AND flow_id IN \
             (SELECT flow_id FROM flows WHERE status IN ('completed', 'dead'))",
            [],
        )?;
        Ok(n as u64)
    }

    /// The `dead` flows, sorted — reported for visibility, never repaired.
    pub fn dead_flow_ids(&self) -> Result<Vec<String>> {
        self.ids("SELECT flow_id FROM flows WHERE status = 'dead' ORDER BY flow_id")
    }

    /// The flow ids [`prune_completed_flows`](Self::prune_completed_flows)
    /// would delete for this cutoff, sorted — the preview/report view.
    pub fn prunable_completed_flow_ids(&self, cutoff_ms: i64) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT flow_id FROM flows WHERE {PRUNABLE} ORDER BY flow_id"
        ))?;
        let rows = stmt
            .query_map(params![cutoff_ms], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Retention: delete `completed` flows older than `cutoff_ms` and their
    /// steps (see module docs for eligibility and the replayability caveat).
    /// Steps go first so the schema's foreign key holds throughout.
    pub fn prune_completed_flows(&self, cutoff_ms: i64) -> Result<PruneOutcome> {
        let steps = self.conn.execute(
            &format!(
                "DELETE FROM steps WHERE flow_id IN (SELECT flow_id FROM flows WHERE {PRUNABLE})"
            ),
            params![cutoff_ms],
        )?;
        let flows = self.conn.execute(
            &format!("DELETE FROM flows WHERE {PRUNABLE}"),
            params![cutoff_ms],
        )?;
        Ok(PruneOutcome {
            flows_deleted: flows as u64,
            steps_deleted: steps as u64,
        })
    }

    /// `PRAGMA wal_checkpoint(TRUNCATE)` — fold the WAL back into the main
    /// file and truncate it, reclaiming space after repairs/pruning.
    pub fn wal_checkpoint(&self) -> Result<()> {
        // The pragma returns a (busy, log, checkpointed) row; the counts are
        // not load-bearing for fsck, only that the checkpoint ran.
        self.conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
        Ok(())
    }

    fn count(&self, sql: &str, now_ms: i64) -> Result<u64> {
        let n: i64 = self
            .conn
            .query_row(sql, params![now_ms], |row| row.get(0))?;
        Ok(u64::try_from(n).unwrap_or(0))
    }

    fn ids(&self, sql: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::time::Duration;

    use crate::clock::ManualClock;
    use crate::journal::Journal;
    use crate::sqlite::SqliteJournal;
    use crate::types::{
        CacheKey, FlowId, FlowStatus, NewFlow, StepKey, StepKind, StepOutcome, StepStatus,
    };
    use tempfile::TempDir;

    const T0: i64 = 1_783_728_000_000;

    fn new_flow(id: &str) -> NewFlow {
        NewFlow {
            flow_id: FlowId::new(id),
            entrypoint: "py:pipeline.ingest:main".to_owned(),
            args_hash: "ah-test".to_owned(),
            code_hash: Some("ch-test".to_owned()),
        }
    }

    fn ok_step(started: i64) -> StepOutcome {
        StepOutcome {
            kind: StepKind::Effect,
            attempt: 1,
            status: StepStatus::Ok,
            payload: Some(vec![0xC0]),
            error_class: None,
            started_at: started,
            ended_at: Some(started + 10),
        }
    }

    fn running_step(started: i64) -> StepOutcome {
        StepOutcome {
            status: StepStatus::Running,
            payload: None,
            ended_at: None,
            ..ok_step(started)
        }
    }

    /// A journal with: a healthy completed flow, an expired + a live cache
    /// row, an orphan step, a dangling lease, and a stale running step.
    fn journal_with_findings(dir: &TempDir) -> std::path::PathBuf {
        let path = dir.path().join("journal.db");
        let clock = ManualClock::new(T0);
        {
            let j = SqliteJournal::open(&path, clock.clone()).unwrap();
            // Healthy completed flow with one terminal step.
            j.begin_flow(&new_flow("01OK")).unwrap();
            j.record_step(&FlowId::new("01OK"), 1, &StepKey::new("a#1"), &ok_step(T0))
                .unwrap();
            j.complete_flow(&FlowId::new("01OK"), FlowStatus::Completed)
                .unwrap();

            // Completed flow with a stale running step.
            j.begin_flow(&new_flow("02STALE")).unwrap();
            j.record_step(
                &FlowId::new("02STALE"),
                1,
                &StepKey::new("b#1"),
                &running_step(T0),
            )
            .unwrap();
            j.complete_flow(&FlowId::new("02STALE"), FlowStatus::Completed)
                .unwrap();

            // Live cache row (expires in the future) + expired one.
            j.put_cache(&CacheKey::new("live"), b"v", Duration::from_hours(1))
                .unwrap();
            j.put_cache(&CacheKey::new("gone"), b"v", Duration::from_secs(1))
                .unwrap();
        }
        // Foreign-writer damage: an orphan step and a dangling lease, written
        // with foreign keys off (a foreign tool need not honour the pragma).
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
        conn.execute(
            "INSERT INTO steps VALUES ('09GHOST', 1, 'x#1', 'effect', 1, 'ok', NULL, NULL, ?1, ?1)",
            params![T0],
        )
        .unwrap();
        conn.execute(
            "UPDATE flows SET lease_holder = 'host-z:pid-9', lease_expires = ?1 \
             WHERE flow_id = '01OK'",
            params![T0 + 60_000],
        )
        .unwrap();
        path
    }

    #[test]
    fn a_healthy_journal_reports_clean() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("journal.db");
        {
            let j = SqliteJournal::open(&path, ManualClock::new(T0)).unwrap();
            j.begin_flow(&new_flow("01OK")).unwrap();
            j.complete_flow(&FlowId::new("01OK"), FlowStatus::Completed)
                .unwrap();
        }
        let admin = JournalAdmin::open_readonly(&path).unwrap();
        assert!(admin.integrity_check().ok);
        assert!(admin.schema_present().unwrap());
        assert_eq!(admin.expired_cache_count(T0).unwrap(), 0);
        assert_eq!(admin.orphan_step_count().unwrap(), 0);
        assert!(admin.orphan_step_flow_ids().unwrap().is_empty());
        assert!(admin.dangling_lease_flow_ids().unwrap().is_empty());
        assert!(admin.stale_running_steps().unwrap().is_empty());
        assert!(admin.dead_flow_ids().unwrap().is_empty());
    }

    #[test]
    fn findings_are_detected_and_repaired() {
        let dir = TempDir::new().unwrap();
        let path = journal_with_findings(&dir);
        let now = T0 + 10_000; // past the 1s cache TTL, within the 1h one

        let admin = JournalAdmin::open_readwrite(&path).unwrap();
        assert!(admin.integrity_check().ok);
        assert_eq!(admin.expired_cache_count(now).unwrap(), 1);
        assert_eq!(admin.orphan_step_count().unwrap(), 1);
        assert_eq!(admin.orphan_step_flow_ids().unwrap(), vec!["09GHOST"]);
        assert_eq!(admin.dangling_lease_flow_ids().unwrap(), vec!["01OK"]);
        assert_eq!(
            admin.stale_running_steps().unwrap(),
            vec![StaleRunningStep {
                flow_id: "02STALE".to_owned(),
                seq: 1
            }]
        );

        // Repairs report their counts…
        assert_eq!(admin.sweep_expired_cache(now).unwrap(), 1);
        assert_eq!(admin.delete_orphan_steps().unwrap(), 1);
        assert_eq!(admin.clear_dangling_leases().unwrap(), 1);
        assert_eq!(admin.sweep_stale_running_steps().unwrap(), 1);
        admin.wal_checkpoint().unwrap();

        // …and a re-check is clean; the live cache row survived.
        assert_eq!(admin.expired_cache_count(now).unwrap(), 0);
        assert_eq!(admin.orphan_step_count().unwrap(), 0);
        assert!(admin.dangling_lease_flow_ids().unwrap().is_empty());
        assert!(admin.stale_running_steps().unwrap().is_empty());
        let j = SqliteJournal::open(&path, ManualClock::new(now)).unwrap();
        assert!(j.get_cache(&CacheKey::new("live")).unwrap().is_some());
    }

    #[test]
    fn lease_repair_does_not_touch_updated_at() {
        let dir = TempDir::new().unwrap();
        let path = journal_with_findings(&dir);
        let before: i64 = Connection::open(&path)
            .unwrap()
            .query_row(
                "SELECT updated_at FROM flows WHERE flow_id = '01OK'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let admin = JournalAdmin::open_readwrite(&path).unwrap();
        assert_eq!(admin.clear_dangling_leases().unwrap(), 1);
        let after: i64 = Connection::open(&path)
            .unwrap()
            .query_row(
                "SELECT updated_at FROM flows WHERE flow_id = '01OK'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(before, after, "repairs must not bump updated_at");
    }

    #[test]
    fn a_non_database_file_folds_into_the_integrity_outcome() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("garbage.db");
        std::fs::write(&path, b"this is not a sqlite database at all........").unwrap();
        let admin = JournalAdmin::open_readonly(&path).unwrap();
        let outcome = admin.integrity_check();
        assert!(!outcome.ok);
        assert!(!outcome.detail.is_empty());
    }

    #[test]
    fn a_foreign_sqlite_file_is_not_schema_present() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("other.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE t (x INTEGER);").unwrap();
        drop(conn);
        let admin = JournalAdmin::open_readonly(&path).unwrap();
        assert!(admin.integrity_check().ok, "valid SQLite, wrong schema");
        assert!(!admin.schema_present().unwrap());
    }

    #[test]
    fn open_readwrite_never_creates_a_file() {
        let dir = TempDir::new().unwrap();
        let missing = dir.path().join("absent.db");
        assert!(JournalAdmin::open_readwrite(&missing).is_err());
        assert!(!missing.exists(), "fsck must not conjure a journal");
    }

    #[test]
    fn prune_deletes_only_old_completed_flows_and_their_steps() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("journal.db");
        let clock = ManualClock::new(T0);
        {
            let j = SqliteJournal::open(&path, clock.clone()).unwrap();
            // Old completed flow (updated at T0) with two steps.
            j.begin_flow(&new_flow("01OLD")).unwrap();
            j.record_step(&FlowId::new("01OLD"), 1, &StepKey::new("a#1"), &ok_step(T0))
                .unwrap();
            j.record_step(&FlowId::new("01OLD"), 2, &StepKey::new("b#1"), &ok_step(T0))
                .unwrap();
            j.complete_flow(&FlowId::new("01OLD"), FlowStatus::Completed)
                .unwrap();

            // Old but NOT completed: running, failed, dead — all kept.
            j.begin_flow(&new_flow("02RUN")).unwrap();
            j.begin_flow(&new_flow("03FAIL")).unwrap();
            j.complete_flow(&FlowId::new("03FAIL"), FlowStatus::Failed)
                .unwrap();
            j.begin_flow(&new_flow("04DEAD")).unwrap();
            j.complete_flow(&FlowId::new("04DEAD"), FlowStatus::Dead)
                .unwrap();

            // Fresh completed flow — newer than the cutoff, kept.
            clock.advance(100_000);
            j.begin_flow(&new_flow("05NEW")).unwrap();
            j.complete_flow(&FlowId::new("05NEW"), FlowStatus::Completed)
                .unwrap();
        }

        let admin = JournalAdmin::open_readwrite(&path).unwrap();
        let cutoff = T0 + 50_000;
        assert_eq!(
            admin.prunable_completed_flow_ids(cutoff).unwrap(),
            vec!["01OLD"]
        );
        let outcome = admin.prune_completed_flows(cutoff).unwrap();
        assert_eq!(
            outcome,
            PruneOutcome {
                flows_deleted: 1,
                steps_deleted: 2
            }
        );
        // Everything else survived.
        let survivors = admin
            .ids("SELECT flow_id FROM flows ORDER BY flow_id")
            .unwrap();
        assert_eq!(survivors, vec!["02RUN", "03FAIL", "04DEAD", "05NEW"]);
        // Idempotent: a second prune finds nothing.
        assert_eq!(
            admin.prune_completed_flows(cutoff).unwrap(),
            PruneOutcome {
                flows_deleted: 0,
                steps_deleted: 0
            }
        );
    }

    #[test]
    fn prune_keeps_completed_flows_with_outbox_rows() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("journal.db");
        {
            let j = SqliteJournal::open(&path, ManualClock::new(T0)).unwrap();
            j.begin_flow(&new_flow("01HANDOFF")).unwrap();
            j.complete_flow(&FlowId::new("01HANDOFF"), FlowStatus::Completed)
                .unwrap();
        }
        let conn = Connection::open(&path).unwrap();
        conn.execute(
            "INSERT INTO outbox (flow_id, destination, payload) \
             VALUES ('01HANDOFF', 'webhook:billing', X'C0')",
            [],
        )
        .unwrap();
        drop(conn);

        let admin = JournalAdmin::open_readwrite(&path).unwrap();
        let cutoff = T0 + 1_000_000;
        assert!(
            admin
                .prunable_completed_flow_ids(cutoff)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            admin.prune_completed_flows(cutoff).unwrap(),
            PruneOutcome {
                flows_deleted: 0,
                steps_deleted: 0
            }
        );
    }
}
