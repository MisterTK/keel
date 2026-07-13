//! The SQLite [`Journal`] backend.
//!
//! WAL-mode SQLite, chosen (architecture spec §4.2) for crash-safe *multi-
//! process* access on one machine and a universally inspectable file. The
//! schema is `contracts/journal.sql`, applied verbatim via `include_str!` on
//! first open — this module never restates it. Every method is one autocommit
//! statement (no explicit transactions; the lease is a single conditional
//! `UPDATE`), so concurrent processes serialize on SQLite's own file locking
//! plus `busy_timeout`, and within a process a `Mutex` serializes the
//! `!Sync` connection.
//!
//! Deliberate v1 simplifications, recorded here per the manifesto:
//! - Expired cache rows are filtered on read; each [`open`](SqliteJournal::open)
//!   sweeps the ones already expired (a bounded, startup-time reap so a
//!   dev-cache-heavy project's journal does not grow without bound between
//!   runs), and `keel fsck --fix` sweeps on demand ([`crate::admin`]).
//! - `incomplete_flows` treats only `running` as resumable; `failed`/`dead`
//!   are terminal for recovery (the flow manager owns the failed→retry policy
//!   if one is ever added).

use core::time::Duration;
use std::path::Path;
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension, params};

use crate::clock::Clock;
use crate::error::{Error, Result};
use crate::journal::Journal;
use crate::types::{
    CacheKey, FlowDescriptor, FlowId, FlowStatus, NewFlow, ProcessId, StepKey, StepKind,
    StepOutcome, StepStatus, error_class_from_db, error_class_str,
};

/// The frozen schema, embedded so the compiled backend and the contract can
/// never drift.
const SCHEMA: &str = include_str!("../contract/journal.sql");

/// Per-connection pragmas set on every open. WAL is also persisted in the file
/// by the schema, but re-asserting it makes opening a foreign-created DB safe.
const CONNECTION_PRAGMAS: &str = "\
PRAGMA journal_mode = WAL;
PRAGMA foreign_keys = ON;
PRAGMA busy_timeout = 5000;
PRAGMA synchronous = NORMAL;";

const FLOW_COLUMNS: &str = "flow_id, entrypoint, args_hash, code_hash, status, lease_holder, lease_expires, \
     created_at, updated_at";

/// A crash-durable [`Journal`] over a single WAL-mode SQLite file, generic over
/// the [`Clock`] that stamps the timestamps the store originates.
pub struct SqliteJournal<C: Clock> {
    conn: Mutex<Connection>,
    clock: C,
}

impl<C: Clock> core::fmt::Debug for SqliteJournal<C> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SqliteJournal")
            .field("clock", &self.clock)
            .finish_non_exhaustive()
    }
}

impl<C: Clock> SqliteJournal<C> {
    /// Open (creating if absent) the journal at `path`, initializing the frozen
    /// schema when the file is new and asserting the connection pragmas either
    /// way. `clock` supplies every timestamp the store originates.
    ///
    /// Schema init is **race-safe and crash-atomic**: it runs inside a single
    /// `BEGIN IMMEDIATE` transaction, re-checking `flows` existence *after*
    /// taking the write lock. Two processes opening a fresh project therefore
    /// serialize — the loser sees the tables the winner created and skips the
    /// batch instead of failing with "table already exists"; and a crash
    /// (`kill -9`, the product's own model) mid-batch rolls the whole transaction
    /// back on next open rather than leaving a half-created schema that every
    /// future open mistakes for complete and then silently fails to journal to.
    pub fn open(path: impl AsRef<Path>, clock: C) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(CONNECTION_PRAGMAS)?;
        init_schema(&conn)?;
        // Opportunistic reap: expired cache rows are invisible to reads but
        // still grow the file; sweeping the backlog once per open bounds a
        // dev-cache-heavy project's journal without touching the hot path.
        // Best-effort — a locked or read-only file must not fail the open.
        let _ = conn.execute(
            "DELETE FROM cache WHERE expires_at <= ?1",
            params![clock.now_ms()],
        );
        Ok(Self {
            conn: Mutex::new(conn),
            clock,
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().expect("journal connection mutex poisoned")
    }

    fn now(&self) -> i64 {
        self.clock.now_ms()
    }
}

impl<C: Clock> Journal for SqliteJournal<C> {
    fn begin_flow(&self, flow: &NewFlow) -> Result<FlowId> {
        let now = self.now();
        let conn = self.lock();
        conn.execute(
            "INSERT INTO flows \
             (flow_id, entrypoint, args_hash, code_hash, status, \
              lease_holder, lease_expires, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, 'running', NULL, NULL, ?5, ?5) \
             ON CONFLICT(flow_id) DO NOTHING",
            params![
                flow.flow_id.as_str(),
                flow.entrypoint,
                flow.args_hash,
                flow.code_hash,
                now,
            ],
        )?;
        Ok(flow.flow_id.clone())
    }

    fn record_step(
        &self,
        flow: &FlowId,
        seq: u64,
        key: &StepKey,
        outcome: &StepOutcome,
    ) -> Result<()> {
        let seq = to_i64("seq", seq)?;
        let conn = self.lock();
        conn.execute(
            "INSERT INTO steps \
             (flow_id, seq, step_key, kind, attempt, outcome, payload, \
              error_class, started_at, ended_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) \
             ON CONFLICT(flow_id, seq) DO UPDATE SET \
               step_key = excluded.step_key, kind = excluded.kind, \
               attempt = excluded.attempt, outcome = excluded.outcome, \
               payload = excluded.payload, error_class = excluded.error_class, \
               ended_at = excluded.ended_at",
            params![
                flow.as_str(),
                seq,
                key.as_str(),
                outcome.kind.as_str(),
                i64::from(outcome.attempt),
                outcome.status.as_str(),
                outcome.payload.as_deref(),
                outcome.error_class.map(error_class_str),
                outcome.started_at,
                outcome.ended_at,
            ],
        )?;
        Ok(())
    }

    fn lookup_step(&self, flow: &FlowId, seq: u64, key: &StepKey) -> Result<Option<StepOutcome>> {
        let seq = to_i64("seq", seq)?;
        let conn = self.lock();
        let row = conn
            .query_row(
                "SELECT kind, attempt, outcome, payload, error_class, started_at, ended_at \
                 FROM steps WHERE flow_id = ?1 AND seq = ?2 AND step_key = ?3",
                params![flow.as_str(), seq, key.as_str()],
                step_row,
            )
            .optional()?;
        row.map(step_from_row).transpose()
    }

    fn step_at(&self, flow: &FlowId, seq: u64) -> Result<Option<(StepKey, StepOutcome)>> {
        let seq = to_i64("seq", seq)?;
        let conn = self.lock();
        let row = conn
            .query_row(
                "SELECT step_key, kind, attempt, outcome, payload, error_class, started_at, ended_at \
                 FROM steps WHERE flow_id = ?1 AND seq = ?2",
                params![flow.as_str(), seq],
                |row| Ok((row.get::<_, String>(0)?, step_row_from(row, 1)?)),
            )
            .optional()?;
        row.map(|(key, raw)| Ok((StepKey::new(key), step_from_row(raw)?)))
            .transpose()
    }

    fn get_flow(&self, flow: &FlowId) -> Result<Option<FlowDescriptor>> {
        let conn = self.lock();
        let row = conn
            .query_row(
                &format!("SELECT {FLOW_COLUMNS} FROM flows WHERE flow_id = ?1"),
                params![flow.as_str()],
                flow_row,
            )
            .optional()?;
        row.map(flow_from_row).transpose()
    }

    fn complete_flow(&self, flow: &FlowId, status: FlowStatus) -> Result<()> {
        let now = self.now();
        let conn = self.lock();
        // A `completed` flow is terminal-success and immutable: never let a later
        // rerun demote it (a designed replay-miss after a code change, or an error
        // while re-running already-finished code) to `failed`/`dead`/`running`,
        // which would reopen a done flow for live re-execution. The front ends
        // already avoid this, but the durable source-of-truth enforces it for any
        // caller (root cause of the completed→failed flow findings). Failed→
        // completed (a resume that finally succeeds) and every other transition
        // stay allowed; a redundant complete(Completed) is a harmless no-op.
        conn.execute(
            "UPDATE flows SET status = ?2, updated_at = ?3, \
             lease_holder = NULL, lease_expires = NULL \
             WHERE flow_id = ?1 AND status != 'completed'",
            params![flow.as_str(), status.as_str(), now],
        )?;
        Ok(())
    }

    fn incomplete_flows(&self, lease_expired: bool) -> Result<Vec<FlowDescriptor>> {
        let now = self.now();
        let sql = if lease_expired {
            format!(
                "SELECT {FLOW_COLUMNS} FROM flows \
                 WHERE status = 'running' AND (lease_expires IS NULL OR lease_expires < ?1) \
                 ORDER BY flow_id"
            )
        } else {
            format!(
                "SELECT {FLOW_COLUMNS} FROM flows \
                 WHERE status = 'running' AND lease_expires IS NOT NULL AND lease_expires >= ?1 \
                 ORDER BY flow_id"
            )
        };
        let conn = self.lock();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params![now], flow_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter().map(flow_from_row).collect()
    }

    fn acquire_lease(&self, flow: &FlowId, holder: &ProcessId, ttl: Duration) -> Result<bool> {
        let now = self.now();
        let expires = now.saturating_add(duration_ms(ttl));
        let conn = self.lock();
        let updated = conn.execute(
            "UPDATE flows SET lease_holder = ?2, lease_expires = ?3, updated_at = ?4 \
             WHERE flow_id = ?1 AND status = 'running' \
               AND (lease_holder IS NULL OR lease_holder = ?2 \
                    OR lease_expires IS NULL OR lease_expires < ?4)",
            params![flow.as_str(), holder.as_str(), expires, now],
        )?;
        Ok(updated == 1)
    }

    fn put_cache(&self, key: &CacheKey, value: &[u8], ttl: Duration) -> Result<()> {
        let expires = self.now().saturating_add(duration_ms(ttl));
        let conn = self.lock();
        conn.execute(
            "INSERT INTO cache (key, value, expires_at) VALUES (?1, ?2, ?3) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, \
             expires_at = excluded.expires_at",
            params![key.as_str(), value, expires],
        )?;
        Ok(())
    }

    fn get_cache(&self, key: &CacheKey) -> Result<Option<Vec<u8>>> {
        let now = self.now();
        let conn = self.lock();
        let value = conn
            .query_row(
                "SELECT value FROM cache WHERE key = ?1 AND expires_at > ?2",
                params![key.as_str(), now],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?;
        Ok(value)
    }
}

/// A flow row exactly as the columns arrive from SQLite, before typing.
struct FlowRowData {
    flow_id: String,
    entrypoint: String,
    args_hash: String,
    code_hash: Option<String>,
    status: String,
    lease_holder: Option<String>,
    lease_expires: Option<i64>,
    created_at: i64,
    updated_at: i64,
}

/// A step row as the columns arrive from SQLite, before typing.
struct StepRowData {
    kind: String,
    attempt: i64,
    outcome: String,
    payload: Option<Vec<u8>>,
    error_class: Option<String>,
    started_at: i64,
    ended_at: Option<i64>,
}

/// Extracts the raw flow columns inside the query closure (which must return a
/// `rusqlite::Result`); typing happens afterwards in [`flow_from_row`].
fn flow_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<FlowRowData> {
    Ok(FlowRowData {
        flow_id: row.get(0)?,
        entrypoint: row.get(1)?,
        args_hash: row.get(2)?,
        code_hash: row.get(3)?,
        status: row.get(4)?,
        lease_holder: row.get(5)?,
        lease_expires: row.get(6)?,
        created_at: row.get(7)?,
        updated_at: row.get(8)?,
    })
}

fn step_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StepRowData> {
    step_row_from(row, 0)
}

/// Extract the seven step columns starting at column index `base`, so a query
/// that prefixes them (e.g. `step_at`, which selects `step_key` first) reuses
/// the same typing.
fn step_row_from(row: &rusqlite::Row<'_>, base: usize) -> rusqlite::Result<StepRowData> {
    Ok(StepRowData {
        kind: row.get(base)?,
        attempt: row.get(base + 1)?,
        outcome: row.get(base + 2)?,
        payload: row.get(base + 3)?,
        error_class: row.get(base + 4)?,
        started_at: row.get(base + 5)?,
        ended_at: row.get(base + 6)?,
    })
}

fn flow_from_row(raw: FlowRowData) -> Result<FlowDescriptor> {
    Ok(FlowDescriptor {
        flow_id: FlowId::new(raw.flow_id),
        entrypoint: raw.entrypoint,
        args_hash: raw.args_hash,
        code_hash: raw.code_hash,
        status: FlowStatus::from_db(&raw.status)?,
        lease_holder: raw.lease_holder.map(ProcessId::new),
        lease_expires: raw.lease_expires,
        created_at: raw.created_at,
        updated_at: raw.updated_at,
    })
}

fn step_from_row(raw: StepRowData) -> Result<StepOutcome> {
    let error_class = raw
        .error_class
        .as_deref()
        .map(|value| error_class_from_db("steps.error_class", value))
        .transpose()?;
    Ok(StepOutcome {
        kind: StepKind::from_db(&raw.kind)?,
        attempt: u32::try_from(raw.attempt)
            .map_err(|_| Error::corrupt("steps.attempt", raw.attempt))?,
        status: StepStatus::from_db(&raw.outcome)?,
        payload: raw.payload,
        error_class,
        started_at: raw.started_at,
        ended_at: raw.ended_at,
    })
}

/// Apply the frozen schema exactly once, race-safely and crash-atomically.
///
/// `BEGIN IMMEDIATE` takes the database write lock up front, so concurrent
/// first-opens serialize here; the `flows` existence check is re-evaluated
/// *inside* the lock so the process that waited sees the winner's tables and
/// skips the batch. The whole `CREATE TABLE` batch commits atomically, so a
/// crash mid-batch rolls back (leaving no `flows` table) and the next open
/// re-applies the full schema — never a partially-initialized journal.
fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let result = (|| {
        if !table_exists(conn, "flows")? {
            conn.execute_batch(SCHEMA)?;
        }
        Ok(())
    })();
    match result {
        Ok(()) => {
            conn.execute_batch("COMMIT")?;
            Ok(())
        }
        Err(e) => {
            // Best-effort rollback; surface the original error.
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

/// Does a table by this name exist in the connection's schema?
fn table_exists(conn: &Connection, name: &str) -> Result<bool> {
    let found = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            params![name],
            |_| Ok(()),
        )
        .optional()?;
    Ok(found.is_some())
}

/// Clamp a `Duration` to whole milliseconds as an `i64` (saturating; a TTL past
/// the epoch's `i64` range is not a real configuration).
fn duration_ms(ttl: Duration) -> i64 {
    i64::try_from(ttl.as_millis()).unwrap_or(i64::MAX)
}

/// Narrow a `u64` sequence number to the schema's `INTEGER` (`i64`) domain.
fn to_i64(column: &'static str, seq: u64) -> Result<i64> {
    i64::try_from(seq).map_err(|_| Error::corrupt(column, seq))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::ManualClock;
    use keel_core_api::ErrorClass;
    use tempfile::TempDir;

    const T0: i64 = 1_783_728_000_000;

    fn journal(dir: &TempDir, clock: ManualClock) -> SqliteJournal<ManualClock> {
        SqliteJournal::open(dir.path().join("journal.db"), clock).expect("open journal")
    }

    fn sample_flow(id: &str) -> NewFlow {
        NewFlow {
            flow_id: FlowId::new(id),
            entrypoint: "py:pipeline.ingest:main".to_owned(),
            args_hash: "ah-test".to_owned(),
            code_hash: Some("ch-test".to_owned()),
        }
    }

    #[test]
    fn begin_flow_is_idempotent_and_stamps_running_from_the_clock() {
        let dir = TempDir::new().unwrap();
        let clock = ManualClock::new(T0);
        let j = journal(&dir, clock.clone());
        let id = FlowId::new("01FLOW");

        j.begin_flow(&sample_flow("01FLOW")).unwrap();
        clock.advance(5_000); // a re-begin must NOT reset created_at/updated_at
        j.begin_flow(&sample_flow("01FLOW")).unwrap();

        let flow = j.get_flow(&id).unwrap().expect("flow present");
        assert_eq!(flow.status, FlowStatus::Running);
        assert_eq!(flow.created_at, T0);
        assert_eq!(flow.updated_at, T0);
        assert!(flow.lease_holder.is_none());
    }

    #[test]
    fn record_step_then_lookup_step_round_trips_every_field() {
        let dir = TempDir::new().unwrap();
        let j = journal(&dir, ManualClock::new(T0));
        let flow = FlowId::new("01FLOW");
        let key = StepKey::new("api.enrich.internal#q2");
        j.begin_flow(&sample_flow("01FLOW")).unwrap();

        let outcome = StepOutcome {
            kind: StepKind::Effect,
            attempt: 2,
            status: StepStatus::Ok,
            payload: Some(vec![0x81, 0xA2, 0x6F, 0x6B, 0xC3]),
            error_class: None,
            started_at: T0 + 10,
            ended_at: Some(T0 + 250),
        };
        j.record_step(&flow, 3, &key, &outcome).unwrap();

        let got = j
            .lookup_step(&flow, 3, &key)
            .unwrap()
            .expect("step present");
        assert_eq!(got, outcome);

        // Wrong key at the same seq is a miss, not a false hit.
        assert!(
            j.lookup_step(&flow, 3, &StepKey::new("other#k"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn step_at_reads_the_recorded_key_regardless_of_match() {
        let dir = TempDir::new().unwrap();
        let j = journal(&dir, ManualClock::new(T0));
        let flow = FlowId::new("01FLOW");
        let key = StepKey::new("api.enrich.internal#q2");
        j.begin_flow(&sample_flow("01FLOW")).unwrap();

        let outcome = StepOutcome {
            kind: StepKind::Effect,
            attempt: 1,
            status: StepStatus::Ok,
            payload: Some(vec![0x81, 0xA2, 0x6F, 0x6B, 0xC3]),
            error_class: None,
            started_at: T0 + 10,
            ended_at: Some(T0 + 250),
        };
        j.record_step(&flow, 3, &key, &outcome).unwrap();

        // Reads the step at that seq and surfaces its true key — the input the
        // flow manager compares against to detect divergence.
        let (got_key, got) = j.step_at(&flow, 3).unwrap().expect("step present");
        assert_eq!(got_key, key);
        assert_eq!(got, outcome);
        // An empty seq is a plain miss, not a divergence.
        assert!(j.step_at(&flow, 9).unwrap().is_none());
    }

    #[test]
    fn record_step_upsert_preserves_started_at_on_running_to_ok() {
        let dir = TempDir::new().unwrap();
        let j = journal(&dir, ManualClock::new(T0));
        let flow = FlowId::new("01FLOW");
        let key = StepKey::new("api.store.internal#w1");
        j.begin_flow(&sample_flow("01FLOW")).unwrap();

        let running = StepOutcome {
            kind: StepKind::Effect,
            attempt: 1,
            status: StepStatus::Running,
            payload: None,
            error_class: None,
            started_at: T0 + 100,
            ended_at: None,
        };
        j.record_step(&flow, 4, &key, &running).unwrap();

        let done = StepOutcome {
            status: StepStatus::Ok,
            payload: Some(vec![0xC0]),
            ended_at: Some(T0 + 900),
            started_at: T0 + 555, // a re-record proposing a later start...
            ..running.clone()
        };
        j.record_step(&flow, 4, &key, &done).unwrap();

        let got = j.lookup_step(&flow, 4, &key).unwrap().unwrap();
        assert_eq!(got.status, StepStatus::Ok);
        assert_eq!(got.started_at, T0 + 100, "original start is preserved");
        assert_eq!(got.ended_at, Some(T0 + 900));
    }

    #[test]
    fn error_step_round_trips_its_error_class() {
        let dir = TempDir::new().unwrap();
        let j = journal(&dir, ManualClock::new(T0));
        let flow = FlowId::new("01FLOW");
        let key = StepKey::new("api.billing.internal#w7");
        j.begin_flow(&sample_flow("01FLOW")).unwrap();

        let outcome = StepOutcome {
            kind: StepKind::Effect,
            attempt: 5,
            status: StepStatus::Error,
            payload: None,
            error_class: Some(ErrorClass::Http),
            started_at: T0,
            ended_at: Some(T0 + 1),
        };
        j.record_step(&flow, 2, &key, &outcome).unwrap();
        let got = j.lookup_step(&flow, 2, &key).unwrap().unwrap();
        assert_eq!(got.error_class, Some(ErrorClass::Http));
        assert_eq!(got.attempt, 5);
    }

    #[test]
    fn complete_flow_moves_status_out_of_the_recovery_set() {
        let dir = TempDir::new().unwrap();
        let clock = ManualClock::new(T0);
        let j = journal(&dir, clock.clone());
        let flow = FlowId::new("01FLOW");
        j.begin_flow(&sample_flow("01FLOW")).unwrap();

        clock.advance(1_000);
        j.complete_flow(&flow, FlowStatus::Completed).unwrap();

        let stored = j.get_flow(&flow).unwrap().unwrap();
        assert_eq!(stored.status, FlowStatus::Completed);
        assert_eq!(stored.updated_at, T0 + 1_000);
        assert!(j.incomplete_flows(true).unwrap().is_empty());
        assert!(j.incomplete_flows(false).unwrap().is_empty());
    }

    #[test]
    fn completed_flow_is_never_demoted() {
        let dir = TempDir::new().unwrap();
        let clock = ManualClock::new(T0);
        let j = journal(&dir, clock.clone());

        // A completed flow must never be reopened by a later rerun.
        let done = FlowId::new("01DONE");
        j.begin_flow(&sample_flow("01DONE")).unwrap();
        clock.advance(1_000);
        j.complete_flow(&done, FlowStatus::Completed).unwrap();
        // A rerun that errors (or a replay-miss) tries to mark it failed…
        clock.advance(5_000);
        j.complete_flow(&done, FlowStatus::Failed).unwrap();
        let stored = j.get_flow(&done).unwrap().unwrap();
        assert_eq!(
            stored.status,
            FlowStatus::Completed,
            "completed must stay completed"
        );
        assert_eq!(
            stored.updated_at,
            T0 + 1_000,
            "blocked demotion must not bump updated_at"
        );
        assert!(
            j.incomplete_flows(true).unwrap().is_empty(),
            "must not reopen for recovery"
        );
        // …and a `dead` demotion is refused too.
        j.complete_flow(&done, FlowStatus::Dead).unwrap();
        assert_eq!(
            j.get_flow(&done).unwrap().unwrap().status,
            FlowStatus::Completed
        );

        // Failed → completed (a resume that finally succeeds) is still allowed.
        let retried = FlowId::new("02RETRY");
        j.begin_flow(&sample_flow("02RETRY")).unwrap();
        j.complete_flow(&retried, FlowStatus::Failed).unwrap();
        j.complete_flow(&retried, FlowStatus::Completed).unwrap();
        assert_eq!(
            j.get_flow(&retried).unwrap().unwrap().status,
            FlowStatus::Completed
        );
    }

    #[test]
    fn incomplete_flows_splits_on_lease_expiry_against_the_clock() {
        let dir = TempDir::new().unwrap();
        let clock = ManualClock::new(T0);
        let j = journal(&dir, clock.clone());
        let flow = FlowId::new("01FLOW");
        let holder = ProcessId::new("host-a:pid-1");
        j.begin_flow(&sample_flow("01FLOW")).unwrap();
        assert!(
            j.acquire_lease(&flow, &holder, Duration::from_secs(30))
                .unwrap()
        );

        // Lease valid: shows as actively-leased, not as a recovery candidate.
        assert!(j.incomplete_flows(true).unwrap().is_empty());
        assert_eq!(j.incomplete_flows(false).unwrap().len(), 1);

        // Past expiry: now a recovery candidate.
        clock.advance(31_000);
        let recoverable = j.incomplete_flows(true).unwrap();
        assert_eq!(recoverable.len(), 1);
        assert_eq!(recoverable[0].flow_id, flow);
        assert!(j.incomplete_flows(false).unwrap().is_empty());
    }

    #[test]
    fn lease_contention_one_holder_wins_until_expiry() {
        // Two handles on one file model two processes; a shared ManualClock
        // (cloned) gives them identical time.
        let dir = TempDir::new().unwrap();
        let clock = ManualClock::new(T0);
        let a = journal(&dir, clock.clone());
        let b = journal(&dir, clock.clone());
        let flow = FlowId::new("01FLOW");
        let holder_a = ProcessId::new("host-a:pid-1");
        let holder_b = ProcessId::new("host-b:pid-2");
        a.begin_flow(&sample_flow("01FLOW")).unwrap();

        assert!(
            a.acquire_lease(&flow, &holder_a, Duration::from_secs(30))
                .unwrap()
        );
        assert!(
            !b.acquire_lease(&flow, &holder_b, Duration::from_secs(30))
                .unwrap(),
            "second process must lose while the lease is valid"
        );
        // The holder may re-take (heartbeat) before expiry.
        assert!(
            a.acquire_lease(&flow, &holder_a, Duration::from_secs(30))
                .unwrap()
        );

        clock.advance(31_000);
        assert!(
            b.acquire_lease(&flow, &holder_b, Duration::from_secs(30))
                .unwrap(),
            "an expired lease is stealable"
        );
    }

    #[test]
    fn cache_put_get_honors_ttl_against_the_injected_clock() {
        let dir = TempDir::new().unwrap();
        let clock = ManualClock::new(T0);
        let j = journal(&dir, clock.clone());
        let key = CacheKey::new("api.catalog.internal#g1");

        j.put_cache(&key, b"payload", Duration::from_mins(1))
            .unwrap();
        clock.advance(30_000);
        assert_eq!(j.get_cache(&key).unwrap().as_deref(), Some(&b"payload"[..]));

        clock.advance(31_000); // now past the 60s TTL
        assert!(j.get_cache(&key).unwrap().is_none());
    }

    #[test]
    fn cache_put_replaces_value_and_extends_expiry() {
        let dir = TempDir::new().unwrap();
        let clock = ManualClock::new(T0);
        let j = journal(&dir, clock.clone());
        let key = CacheKey::new("k");

        j.put_cache(&key, b"v1", Duration::from_secs(10)).unwrap();
        clock.advance(5_000);
        j.put_cache(&key, b"v2", Duration::from_secs(10)).unwrap();
        clock.advance(6_000); // past v1's expiry, within v2's
        assert_eq!(j.get_cache(&key).unwrap().as_deref(), Some(&b"v2"[..]));
    }

    #[test]
    fn open_sweeps_the_expired_cache_backlog() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("journal.db");
        {
            let j = SqliteJournal::open(&path, ManualClock::new(T0)).unwrap();
            j.put_cache(&CacheKey::new("stale"), b"v", Duration::from_secs(1))
                .unwrap();
            j.put_cache(&CacheKey::new("fresh"), b"v", Duration::from_secs(3600))
                .unwrap();
        }
        // Reopen well past the short TTL: the expired row is physically gone,
        // the live one is untouched.
        let reopened = SqliteJournal::open(&path, ManualClock::new(T0 + 10_000)).unwrap();
        assert!(reopened.get_cache(&CacheKey::new("fresh")).unwrap().is_some());
        let remaining: i64 = reopened
            .lock()
            .query_row("SELECT COUNT(*) FROM cache", [], |row| row.get(0))
            .unwrap();
        assert_eq!(remaining, 1, "expired backlog swept on open");
    }

    #[test]
    fn concurrent_first_open_is_race_safe() {
        // Many processes opening a fresh project at once must all succeed: the
        // loser sees the winner's tables and skips the batch, never failing with
        // "table keel_meta already exists" (finding: schema init not race-safe).
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("journal.db");
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let p = path.clone();
                std::thread::spawn(move || {
                    SqliteJournal::open(&p, ManualClock::new(T0))
                        .map(|_| ())
                        .map_err(|e| e.to_string())
                })
            })
            .collect();
        for h in handles {
            h.join()
                .unwrap()
                .expect("concurrent first open must not race on schema creation");
        }
        // The schema is fully present and usable.
        let j = journal(&dir, ManualClock::new(T0));
        j.begin_flow(&sample_flow("01FLOW")).unwrap();
        assert!(j.get_flow(&FlowId::new("01FLOW")).unwrap().is_some());
    }

    #[test]
    fn reopening_an_existing_journal_keeps_its_rows() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("journal.db");
        {
            let j = SqliteJournal::open(&path, ManualClock::new(T0)).unwrap();
            j.begin_flow(&sample_flow("01FLOW")).unwrap();
        }
        let reopened = SqliteJournal::open(&path, ManualClock::new(T0)).unwrap();
        assert!(reopened.get_flow(&FlowId::new("01FLOW")).unwrap().is_some());
    }
}
