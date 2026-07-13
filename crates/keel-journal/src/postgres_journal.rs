//! The Postgres [`Journal`] backend (Level 3 / team-fleet durability,
//! architecture spec §6: "journal = `postgres://…`, any replica recovers any
//! flow").
//!
//! Unlike [`SqliteJournal`](crate::SqliteJournal), which is generic over an
//! injected [`Clock`](crate::Clock) so tests can control time, this backend is
//! **not** parameterized by a local clock at all: every timestamp it
//! originates (`created_at`/`updated_at`, lease expiry, cache expiry) is read
//! from the Postgres server itself (`clock_timestamp()`), never from the
//! connecting process. That is the entire point of a fleet journal — lease
//! arbitration across many hosts is only safe against one canonical clock, and
//! a single conditional `UPDATE` evaluated against `clock_timestamp()` in the
//! same statement is what makes [`acquire_lease`](PostgresJournal::acquire_lease)
//! race-safe between processes on different machines with different (and
//! drifting) system clocks.
//!
//! # Why every connection lives on its own dedicated thread
//!
//! The synchronous `postgres` crate (`postgres::Client`) is a facade over
//! `tokio-postgres`: every blocking call — including, critically, the work its
//! `Drop` impl does to close the connection gracefully — runs by calling
//! `.block_on(..)` on a **private Tokio runtime it owns**. Tokio forbids
//! calling `block_on` on *any* runtime from a thread that is already
//! executing inside *another* runtime's context (it panics: "Cannot start a
//! runtime from within a runtime"), and that includes a value's `Drop` impl
//! running as an async task unwinds. `keel-core`'s `Engine` is an async Tokio
//! engine, so a `postgres::Client` (or anything holding one) that happens to
//! be dropped on one of the engine's own worker threads — e.g. because the
//! last `Arc<dyn Journal>` reference was released inside `execute`, or a
//! reconfigure replaced this journal — would abort the process.
//!
//! The fix: every `postgres::Client` this module ever creates is opened *and
//! dropped* on a plain `std::thread` this module spawns and fully owns (a
//! [`Worker`]), never on a thread whose ambient Tokio context we don't
//! control. [`Journal`] calls dispatch a job to a worker over a channel and
//! block (a plain OS wait, not a Tokio `block_on`) for its reply — safe from
//! any calling context, exactly like `SqliteJournal`'s blocking `rusqlite`
//! calls already are. [`WORKER_COUNT`] workers, each with its own connection,
//! give real concurrency across many flows journaling at once, in place of an
//! async connection pool a synchronous `Journal` trait has no way to await.
//!
//! Deliberate v1 simplification, recorded here per the manifesto: connections
//! are plaintext (`NoTls`). A fleet crossing an untrusted network needs TLS;
//! that is follow-up work (a `sslmode=require` URL would need a
//! `postgres_native_tls` or `postgres_openssl` connector swapped in here), not
//! implemented in this slice.

use core::time::Duration;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc as std_mpsc;
use std::thread;

use postgres::NoTls;

use crate::convert::{
    FLOW_COLUMNS, FlowRowData, StepRowData, duration_ms, flow_from_row, step_from_row, to_i64,
};
use crate::error::{Error, Result};
use crate::journal::Journal;
use crate::types::{
    CacheKey, FlowDescriptor, FlowId, FlowStatus, NewFlow, ProcessId, StepKey, StepOutcome,
    error_class_str,
};

/// The Postgres flavor of `contracts/journal.sql`, applied idempotently on
/// first connection. Not the frozen contract (that's SQLite-only) — see
/// `postgres_schema.sql`'s own header.
const SCHEMA: &str = include_str!("postgres_schema.sql");

/// A SQL expression yielding "now" as milliseconds since the Unix epoch,
/// evaluated by the **server** (`clock_timestamp()`, not `now()`/
/// `current_timestamp`, which are frozen at transaction start and would give
/// every statement in a multi-statement transaction the same instant — every
/// call here is a single autocommit statement, so this is purely "use the
/// server's real-time clock, not the connecting process's").
const NOW_MS: &str = "(EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::BIGINT";

/// Dedicated worker threads (and connections) per [`PostgresJournal`]. Small
/// on purpose: each journal method is one short round trip, so a handful of
/// connections comfortably serves a single process's concurrent flows without
/// exhausting the server's `max_connections` when many processes in the fleet
/// each open their own journal.
const WORKER_COUNT: usize = 8;

/// Fixed key for the `pg_advisory_lock` that serializes first-connect schema
/// creation across a fleet, so concurrent processes never race
/// `CREATE TABLE`/`CREATE INDEX` against a fresh database (see
/// [`init_schema`]). An arbitrary constant, not derived from anything —
/// picked once and never to change, since two different keys would defeat the
/// mutual exclusion.
const SCHEMA_LOCK_KEY: i64 = 0x4B45_454C_4A52_4E4C; // "KEELJRNL", ASCII-derived

/// A job dispatched to a [`Worker`]: run against its owned connection, report
/// the result over whatever channel the caller closed over.
type Job = Box<dyn FnOnce(&mut postgres::Client) + Send>;

/// One dedicated thread owning one `postgres::Client`. The thread — never the
/// caller — opens and closes the connection, so `postgres::Client`'s
/// block-on-a-private-runtime `Drop` never nests inside a caller's Tokio
/// runtime (see module doc).
struct Worker {
    tx: Option<std_mpsc::Sender<Job>>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Worker {
    /// Spawn the thread and connect. Blocks (a plain OS channel wait, safe
    /// from any context) until the connection succeeds or fails, so a bad
    /// URL or an unreachable server fails here, not on the first journal
    /// call.
    fn spawn(config: &postgres::Config) -> Result<Self> {
        let config = config.clone();
        let (tx, rx) = std_mpsc::channel::<Job>();
        let (ready_tx, ready_rx) = std_mpsc::channel::<Result<()>>();
        let handle = thread::Builder::new()
            .name("keel-postgres-journal".to_owned())
            .spawn(move || {
                let mut client = match config.connect(NoTls) {
                    Ok(client) => client,
                    Err(e) => {
                        let _ = ready_tx.send(Err(Error::from(e)));
                        return;
                    }
                };
                if ready_tx.send(Ok(())).is_err() {
                    return; // the opener gave up already
                }
                while let Ok(job) = rx.recv() {
                    job(&mut client);
                }
                // `client` (and the private Tokio runtime it owns) drops
                // here, on this thread — never inside a caller's runtime.
            })
            .map_err(Error::WorkerSpawnFailed)?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                tx: Some(tx),
                handle: Some(handle),
            }),
            Ok(Err(e)) => {
                let _ = handle.join();
                Err(e)
            }
            Err(_) => {
                let _ = handle.join();
                Err(Error::WorkerUnavailable)
            }
        }
    }

    fn send(&self, job: Job) -> Result<()> {
        self.tx
            .as_ref()
            .ok_or(Error::WorkerUnavailable)?
            .send(job)
            .map_err(|_| Error::WorkerUnavailable)
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        // Close the channel first so the worker's `recv` loop ends, *then*
        // join — waiting for the connection to finish closing on the
        // worker's own thread, never this one.
        drop(self.tx.take());
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// A crash-durable [`Journal`] over a Postgres database, shared by every
/// process in a fleet that points its `journal` policy key at the same
/// `postgres://` location.
pub struct PostgresJournal {
    workers: Vec<Worker>,
    next: AtomicUsize,
}

impl core::fmt::Debug for PostgresJournal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PostgresJournal").finish_non_exhaustive()
    }
}

impl PostgresJournal {
    /// Connect [`WORKER_COUNT`] dedicated connections and apply the schema if
    /// this is a fresh database. `url` is a full `postgres://` (or
    /// `postgresql://`) connection string; parsing and every connection
    /// attempt happen here, so a bad URL or an unreachable server fails at
    /// open, not on the first journal call.
    pub fn open(url: &str) -> Result<Self> {
        let mut config: postgres::Config = url.parse()?;
        // Bound connect latency so a mistyped host/unreachable server fails
        // `Engine::configure` in seconds, not the underlying driver's default
        // (much longer) timeout — unless the URL already asked for a specific
        // `connect_timeout`, which wins.
        if config.get_connect_timeout().is_none() {
            config.connect_timeout(Duration::from_secs(10));
        }

        let mut workers = Vec::with_capacity(WORKER_COUNT);
        for _ in 0..WORKER_COUNT {
            workers.push(Worker::spawn(&config)?); // early return drops any already-spawned workers
        }
        let journal = Self {
            workers,
            next: AtomicUsize::new(0),
        };

        journal.exec(init_schema)?;
        // Opportunistic reap of the expired-cache backlog, mirroring
        // SqliteJournal::open — best-effort, never fails the open.
        let _ = journal.exec(|client| {
            client.execute(
                &format!("DELETE FROM cache WHERE expires_at <= {NOW_MS}"),
                &[],
            )?;
            Ok(())
        });
        Ok(journal)
    }

    /// Dispatch `f` to the next worker (round-robin) and block for its
    /// result. The wait is a plain OS channel receive, not a Tokio
    /// `block_on`, so this is safe to call from any context, including a
    /// Tokio worker thread.
    fn exec<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut postgres::Client) -> Result<T> + Send + 'static,
    {
        let idx = self.next.fetch_add(1, Ordering::Relaxed) % self.workers.len();
        let (tx, rx) = std_mpsc::channel::<Result<T>>();
        let job: Job = Box::new(move |client| {
            let _ = tx.send(f(client));
        });
        self.workers[idx].send(job)?;
        rx.recv().map_err(|_| Error::WorkerUnavailable)?
    }
}

impl Journal for PostgresJournal {
    fn begin_flow(&self, flow: &NewFlow) -> Result<FlowId> {
        let flow_id = flow.flow_id.clone();
        let flow_id_s = flow.flow_id.as_str().to_owned();
        let entrypoint = flow.entrypoint.clone();
        let args_hash = flow.args_hash.clone();
        let code_hash = flow.code_hash.clone();
        self.exec(move |client| {
            client.execute(
                &format!(
                    "WITH t AS (SELECT {NOW_MS} AS now_ms) \
                     INSERT INTO flows \
                     (flow_id, entrypoint, args_hash, code_hash, status, \
                      lease_holder, lease_expires, created_at, updated_at) \
                     SELECT $1, $2, $3, $4, 'running', NULL, NULL, t.now_ms, t.now_ms FROM t \
                     ON CONFLICT (flow_id) DO NOTHING"
                ),
                &[&flow_id_s, &entrypoint, &args_hash, &code_hash],
            )?;
            Ok(())
        })?;
        Ok(flow_id)
    }

    fn record_step(
        &self,
        flow: &FlowId,
        seq: u64,
        key: &StepKey,
        outcome: &StepOutcome,
    ) -> Result<()> {
        let seq = to_i64("seq", seq)?;
        let flow_s = flow.as_str().to_owned();
        let key_s = key.as_str().to_owned();
        let kind = outcome.kind.as_str();
        let attempt = i64::from(outcome.attempt);
        let status = outcome.status.as_str();
        let payload = outcome.payload.clone();
        let error_class = outcome.error_class.map(error_class_str);
        let started_at = outcome.started_at;
        let ended_at = outcome.ended_at;
        self.exec(move |client| {
            client.execute(
                "INSERT INTO steps \
                 (flow_id, seq, step_key, kind, attempt, outcome, payload, \
                  error_class, started_at, ended_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
                 ON CONFLICT (flow_id, seq) DO UPDATE SET \
                   step_key = excluded.step_key, kind = excluded.kind, \
                   attempt = excluded.attempt, outcome = excluded.outcome, \
                   payload = excluded.payload, error_class = excluded.error_class, \
                   ended_at = excluded.ended_at",
                &[
                    &flow_s,
                    &seq,
                    &key_s,
                    &kind,
                    &attempt,
                    &status,
                    &payload,
                    &error_class,
                    &started_at,
                    &ended_at,
                ],
            )?;
            Ok(())
        })
    }

    fn lookup_step(&self, flow: &FlowId, seq: u64, key: &StepKey) -> Result<Option<StepOutcome>> {
        let seq = to_i64("seq", seq)?;
        let flow_s = flow.as_str().to_owned();
        let key_s = key.as_str().to_owned();
        self.exec(move |client| {
            let row = client.query_opt(
                "SELECT kind, attempt, outcome, payload, error_class, started_at, ended_at \
                 FROM steps WHERE flow_id = $1 AND seq = $2 AND step_key = $3",
                &[&flow_s, &seq, &key_s],
            )?;
            row.map(|r| step_from_row(step_row(&r)?)).transpose()
        })
    }

    fn step_at(&self, flow: &FlowId, seq: u64) -> Result<Option<(StepKey, StepOutcome)>> {
        let seq = to_i64("seq", seq)?;
        let flow_s = flow.as_str().to_owned();
        self.exec(move |client| {
            let row = client.query_opt(
                "SELECT step_key, kind, attempt, outcome, payload, error_class, started_at, ended_at \
                 FROM steps WHERE flow_id = $1 AND seq = $2",
                &[&flow_s, &seq],
            )?;
            row.map(|r| {
                let key: String = r.try_get(0)?;
                Ok((StepKey::new(key), step_from_row(step_row_from(&r, 1)?)?))
            })
            .transpose()
        })
    }

    fn get_flow(&self, flow: &FlowId) -> Result<Option<FlowDescriptor>> {
        let flow_s = flow.as_str().to_owned();
        self.exec(move |client| {
            let row = client.query_opt(
                &format!("SELECT {FLOW_COLUMNS} FROM flows WHERE flow_id = $1"),
                &[&flow_s],
            )?;
            row.map(|r| flow_from_row(flow_row(&r)?)).transpose()
        })
    }

    fn complete_flow(&self, flow: &FlowId, status: FlowStatus) -> Result<()> {
        let flow_s = flow.as_str().to_owned();
        let status_s = status.as_str();
        self.exec(move |client| {
            // See SqliteJournal::complete_flow: a `completed` flow is
            // terminal-success and immutable, so the same
            // `status != 'completed'` guard applies here.
            client.execute(
                &format!(
                    "WITH t AS (SELECT {NOW_MS} AS now_ms) \
                     UPDATE flows SET status = $2, updated_at = t.now_ms, \
                     lease_holder = NULL, lease_expires = NULL \
                     FROM t WHERE flow_id = $1 AND status != 'completed'"
                ),
                &[&flow_s, &status_s],
            )?;
            Ok(())
        })
    }

    fn incomplete_flows(&self, lease_expired: bool) -> Result<Vec<FlowDescriptor>> {
        self.exec(move |client| {
            let sql = if lease_expired {
                format!(
                    "SELECT {FLOW_COLUMNS} FROM flows \
                     WHERE status = 'running' AND (lease_expires IS NULL OR lease_expires < {NOW_MS}) \
                     ORDER BY flow_id"
                )
            } else {
                format!(
                    "SELECT {FLOW_COLUMNS} FROM flows \
                     WHERE status = 'running' AND lease_expires IS NOT NULL \
                       AND lease_expires >= {NOW_MS} \
                     ORDER BY flow_id"
                )
            };
            let rows = client.query(&sql, &[])?;
            rows.iter().map(|r| flow_from_row(flow_row(r)?)).collect()
        })
    }

    fn acquire_lease(&self, flow: &FlowId, holder: &ProcessId, ttl: Duration) -> Result<bool> {
        let flow_s = flow.as_str().to_owned();
        let holder_s = holder.as_str().to_owned();
        let ttl_ms = duration_ms(ttl);
        self.exec(move |client| {
            let updated = client.execute(
                &format!(
                    "WITH t AS (SELECT {NOW_MS} AS now_ms) \
                     UPDATE flows SET lease_holder = $2, lease_expires = t.now_ms + $3, \
                       updated_at = t.now_ms \
                     FROM t WHERE flow_id = $1 AND status = 'running' \
                       AND (lease_holder IS NULL OR lease_holder = $2 \
                            OR lease_expires IS NULL OR lease_expires < t.now_ms)"
                ),
                &[&flow_s, &holder_s, &ttl_ms],
            )?;
            Ok(updated == 1)
        })
    }

    fn put_cache(&self, key: &CacheKey, value: &[u8], ttl: Duration) -> Result<()> {
        let key_s = key.as_str().to_owned();
        let value = value.to_vec();
        let ttl_ms = duration_ms(ttl);
        self.exec(move |client| {
            client.execute(
                &format!(
                    "WITH t AS (SELECT {NOW_MS} AS now_ms) \
                     INSERT INTO cache (key, value, expires_at) \
                     SELECT $1, $2, t.now_ms + $3 FROM t \
                     ON CONFLICT (key) DO UPDATE SET value = excluded.value, \
                     expires_at = excluded.expires_at"
                ),
                &[&key_s, &value, &ttl_ms],
            )?;
            Ok(())
        })
    }

    fn get_cache(&self, key: &CacheKey) -> Result<Option<Vec<u8>>> {
        let key_s = key.as_str().to_owned();
        self.exec(move |client| {
            let row = client.query_opt(
                &format!("SELECT value FROM cache WHERE key = $1 AND expires_at > {NOW_MS}"),
                &[&key_s],
            )?;
            row.map(|r| r.try_get::<_, Vec<u8>>(0))
                .transpose()
                .map_err(Error::from)
        })
    }
}

/// Extract the nine `flows` columns ([`FLOW_COLUMNS`]'s order) from a row.
fn flow_row(row: &postgres::Row) -> Result<FlowRowData> {
    Ok(FlowRowData {
        flow_id: row.try_get(0)?,
        entrypoint: row.try_get(1)?,
        args_hash: row.try_get(2)?,
        code_hash: row.try_get(3)?,
        status: row.try_get(4)?,
        lease_holder: row.try_get(5)?,
        lease_expires: row.try_get(6)?,
        created_at: row.try_get(7)?,
        updated_at: row.try_get(8)?,
    })
}

fn step_row(row: &postgres::Row) -> Result<StepRowData> {
    step_row_from(row, 0)
}

/// Extract the seven step columns starting at column index `base`, so a query
/// that prefixes them (e.g. `step_at`, which selects `step_key` first) reuses
/// the same typing.
fn step_row_from(row: &postgres::Row, base: usize) -> Result<StepRowData> {
    Ok(StepRowData {
        kind: row.try_get(base)?,
        attempt: row.try_get(base + 1)?,
        outcome: row.try_get(base + 2)?,
        payload: row.try_get(base + 3)?,
        error_class: row.try_get(base + 4)?,
        started_at: row.try_get(base + 5)?,
        ended_at: row.try_get(base + 6)?,
    })
}

/// Apply the schema exactly once, race-safely across every process in the
/// fleet that might connect to a fresh database at the same time.
///
/// SQLite's backend gets this for free from `BEGIN IMMEDIATE` taking the
/// whole-file write lock; Postgres has no equivalent implicit lock for DDL
/// against a database only some tables of which may exist yet, so a session
/// [`pg_advisory_lock`] stands in for it: every connecting process serializes
/// on the same fixed key before running the (fully idempotent, `IF NOT
/// EXISTS`-guarded) schema batch, so the loser sees the winner's tables and
/// the batch is a no-op instead of racing `CREATE TABLE`.
fn init_schema(client: &mut postgres::Client) -> Result<()> {
    client.execute("SELECT pg_advisory_lock($1)", &[&SCHEMA_LOCK_KEY])?;
    let result = client.batch_execute(SCHEMA);
    // Best-effort unlock even on failure — an error here must not mask the
    // schema failure, but must not leave the session holding the lock either.
    let _ = client.execute("SELECT pg_advisory_unlock($1)", &[&SCHEMA_LOCK_KEY]);
    result.map_err(Error::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Config` parsing rejects nonsense the same way `postgres::Config`'s
    /// `FromStr` does — this is a thin smoke test that `open` surfaces that
    /// failure as our `Error::Postgres`, not a panic. It never reaches the
    /// network: parsing fails before any worker thread is spawned.
    #[test]
    fn open_rejects_a_malformed_url_without_connecting() {
        let err = PostgresJournal::open("not a postgres url").unwrap_err();
        assert!(matches!(err, Error::Postgres(_)));
    }
}
