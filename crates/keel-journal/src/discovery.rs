//! The discovery store: the third evidence source behind `keel init`,
//! `keel status`, and `keel doctor` (DX spec §2).
//!
//! Where the journal records *flows*, this records *traffic*: per-target
//! running aggregates cheap enough to update on every intercepted call, so a
//! session accumulates the evidence needed to propose (or grade) a policy.
//! Its schema is **not** contract-frozen — this module owns it (schema
//! version [`DISCOVERY_SCHEMA_VERSION`], stamped in `PRAGMA user_version`),
//! and here it is:
//!
//! ```sql
//! CREATE TABLE discovery (
//!     target            TEXT PRIMARY KEY,
//!     calls             INTEGER NOT NULL DEFAULT 0,  -- intercepted calls
//!     attempts          INTEGER NOT NULL DEFAULT 0,  -- upstream attempts (Σ)
//!     retries           INTEGER NOT NULL DEFAULT 0,  -- attempts beyond the 1st
//!     successes         INTEGER NOT NULL DEFAULT 0,  -- upstream successes
//!     failures          INTEGER NOT NULL DEFAULT 0,  -- terminal failures
//!     cache_hits        INTEGER NOT NULL DEFAULT 0,  -- served from cache
//!     throttled         INTEGER NOT NULL DEFAULT 0,  -- calls the limiter delayed
//!     breaker_opens     INTEGER NOT NULL DEFAULT 0,  -- calls that saw an open breaker
//!     total_latency_ms  INTEGER NOT NULL DEFAULT 0,  -- Σ latency, for the mean
//!     max_latency_ms    INTEGER NOT NULL DEFAULT 0,  -- worst observed latency
//!     first_seen_ms     INTEGER NOT NULL,
//!     last_seen_ms      INTEGER NOT NULL,
//!     last_error_class  TEXT,                        -- most recent error's class
//!     last_error_status INTEGER,                     -- …and its HTTP status
//!     not_retried       INTEGER NOT NULL DEFAULT 0,  -- KEEL-E014: observed, not retried
//!     unwrapped_calls   INTEGER NOT NULL DEFAULT 0   -- calls with no [target] policy entry
//! ) WITHOUT ROWID;
//!
//! CREATE TABLE discovery_daily (                     -- rolling daily buckets
//!     target          TEXT NOT NULL,                 -- (kept RETENTION_DAYS days)
//!     day             INTEGER NOT NULL,              -- UTC day index: ms / 86_400_000
//!     calls           INTEGER NOT NULL DEFAULT 0,
//!     attempts        INTEGER NOT NULL DEFAULT 0,
//!     retries         INTEGER NOT NULL DEFAULT 0,
//!     successes       INTEGER NOT NULL DEFAULT 0,
//!     failures        INTEGER NOT NULL DEFAULT 0,
//!     cache_hits      INTEGER NOT NULL DEFAULT 0,
//!     throttled       INTEGER NOT NULL DEFAULT 0,
//!     breaker_opens   INTEGER NOT NULL DEFAULT 0,
//!     not_retried     INTEGER NOT NULL DEFAULT 0,
//!     unwrapped_calls INTEGER NOT NULL DEFAULT 0,
//!     PRIMARY KEY (target, day)
//! ) WITHOUT ROWID;
//! ```
//!
//! Accounting: a cache hit is a `call` and a `cache_hit` only — it consumed no
//! upstream attempt, so it is neither a `success` nor a `failure`; thus
//! `calls == successes + failures + cache_hits`. `last_error_*` tracks the most
//! recent error and assumes records/merges arrive roughly time-ordered (they
//! do: `last_seen_ms` is monotonic under a forward-moving clock). `not_retried`
//! counts calls that resolved KEEL-E014 (failed, and Keel refused to retry
//! because the call is not idempotent — the DX Level 0 hard rule's "observed,
//! not retried"). `unwrapped_calls` counts calls that ran with **no explicit
//! `[target."…"]` policy entry** (defaults-only or nothing) — the honest
//! coverage gap `keel status` reports.
//!
//! The `discovery_daily` buckets make time-windowed answers ("retries saved
//! this week") real windows instead of lifetime totals: each recorded call also
//! lands in its target's bucket for the writer-clock UTC day, and buckets older
//! than [`RETENTION_DAYS`] are pruned on the write path. Readers key windows on
//! *stored* days (never wall-clock labels), so window output is a pure function
//! of the file.
//!
//! Migration: files written by the previous (v1, `user_version = 0`) schema are
//! upgraded in place on the first read-write open — the two counter columns are
//! appended (`ALTER TABLE … ADD COLUMN`, so column order matches a fresh v2
//! file), the daily table is created, and `user_version` is stamped. Read-only
//! opens never migrate: on a v1 file the snapshot fills the new counters with
//! zero and the daily snapshot is empty. Old writers keep working against a v2
//! file (their 15-column INSERT leaves the new columns at their defaults).
//!
//! Every mutation is a single UPSERT (one per table), so two processes
//! recording into one file accumulate correctly without a transaction.

use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, Ordering};

use keel_core_api::ErrorClass;
use rusqlite::{Connection, OpenFlags, params};
use serde::Serialize;

use crate::clock::Clock;
use crate::error::{Error, Result};
use crate::types::{error_class_from_db, error_class_str};

/// Current discovery schema version, stamped in `PRAGMA user_version`.
/// Version 0 is the legacy v1 schema (no counter columns, no daily table).
pub const DISCOVERY_SCHEMA_VERSION: i64 = 2;

/// How many trailing UTC days of `discovery_daily` buckets are kept (the
/// current day plus `RETENTION_DAYS - 1` before it). Weekly windows need 7;
/// the slack leaves room for month-scale windows without a schema change.
pub const RETENTION_DAYS: i64 = 30;

/// Milliseconds per UTC day; `day = now_ms / MS_PER_DAY` is the bucket key.
pub const MS_PER_DAY: i64 = 86_400_000;

const DISCOVERY_SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS discovery (
    target            TEXT PRIMARY KEY,
    calls             INTEGER NOT NULL DEFAULT 0,
    attempts          INTEGER NOT NULL DEFAULT 0,
    retries           INTEGER NOT NULL DEFAULT 0,
    successes         INTEGER NOT NULL DEFAULT 0,
    failures          INTEGER NOT NULL DEFAULT 0,
    cache_hits        INTEGER NOT NULL DEFAULT 0,
    throttled         INTEGER NOT NULL DEFAULT 0,
    breaker_opens     INTEGER NOT NULL DEFAULT 0,
    total_latency_ms  INTEGER NOT NULL DEFAULT 0,
    max_latency_ms    INTEGER NOT NULL DEFAULT 0,
    first_seen_ms     INTEGER NOT NULL,
    last_seen_ms      INTEGER NOT NULL,
    last_error_class  TEXT,
    last_error_status INTEGER,
    not_retried       INTEGER NOT NULL DEFAULT 0,
    unwrapped_calls   INTEGER NOT NULL DEFAULT 0
) WITHOUT ROWID;";

const DAILY_SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS discovery_daily (
    target          TEXT NOT NULL,
    day             INTEGER NOT NULL,
    calls           INTEGER NOT NULL DEFAULT 0,
    attempts        INTEGER NOT NULL DEFAULT 0,
    retries         INTEGER NOT NULL DEFAULT 0,
    successes       INTEGER NOT NULL DEFAULT 0,
    failures        INTEGER NOT NULL DEFAULT 0,
    cache_hits      INTEGER NOT NULL DEFAULT 0,
    throttled       INTEGER NOT NULL DEFAULT 0,
    breaker_opens   INTEGER NOT NULL DEFAULT 0,
    not_retried     INTEGER NOT NULL DEFAULT 0,
    unwrapped_calls INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (target, day)
) WITHOUT ROWID;";

const CONNECTION_PRAGMAS: &str = "\
PRAGMA journal_mode = WAL;
PRAGMA busy_timeout = 5000;
PRAGMA synchronous = NORMAL;";

/// The upsert both [`DiscoveryStore::record`] and [`DiscoveryStore::merge_report`]
/// funnel through: insert the row, or add its counters onto the existing one.
/// `max`/`min` keep the extremes; `first_seen` only ever shrinks, `last_seen`
/// only grows; the error columns move together, gated on the incoming row
/// actually carrying an error.
const UPSERT: &str = "\
INSERT INTO discovery
    (target, calls, attempts, retries, successes, failures, cache_hits,
     throttled, breaker_opens, total_latency_ms, max_latency_ms,
     first_seen_ms, last_seen_ms, last_error_class, last_error_status,
     not_retried, unwrapped_calls)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)
ON CONFLICT(target) DO UPDATE SET
    calls            = calls + excluded.calls,
    attempts         = attempts + excluded.attempts,
    retries          = retries + excluded.retries,
    successes        = successes + excluded.successes,
    failures         = failures + excluded.failures,
    cache_hits       = cache_hits + excluded.cache_hits,
    throttled        = throttled + excluded.throttled,
    breaker_opens    = breaker_opens + excluded.breaker_opens,
    total_latency_ms = total_latency_ms + excluded.total_latency_ms,
    max_latency_ms   = max(max_latency_ms, excluded.max_latency_ms),
    first_seen_ms    = min(first_seen_ms, excluded.first_seen_ms),
    last_seen_ms     = max(last_seen_ms, excluded.last_seen_ms),
    last_error_class = coalesce(excluded.last_error_class, last_error_class),
    last_error_status = CASE
        WHEN excluded.last_error_class IS NOT NULL THEN excluded.last_error_status
        ELSE last_error_status END,
    not_retried      = not_retried + excluded.not_retried,
    unwrapped_calls  = unwrapped_calls + excluded.unwrapped_calls";

/// The daily-bucket twin of [`UPSERT`]: pure counter addition per (target, day).
const DAILY_UPSERT: &str = "\
INSERT INTO discovery_daily
    (target, day, calls, attempts, retries, successes, failures, cache_hits,
     throttled, breaker_opens, not_retried, unwrapped_calls)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
ON CONFLICT(target, day) DO UPDATE SET
    calls           = calls + excluded.calls,
    attempts        = attempts + excluded.attempts,
    retries         = retries + excluded.retries,
    successes       = successes + excluded.successes,
    failures        = failures + excluded.failures,
    cache_hits      = cache_hits + excluded.cache_hits,
    throttled       = throttled + excluded.throttled,
    breaker_opens   = breaker_opens + excluded.breaker_opens,
    not_retried     = not_retried + excluded.not_retried,
    unwrapped_calls = unwrapped_calls + excluded.unwrapped_calls";

/// How one intercepted call resolved, from the discovery store's point of view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallResult {
    /// An upstream attempt succeeded.
    Success,
    /// The call failed terminally (attempts exhausted / non-retryable).
    Failure,
    /// The response came from cache; no upstream attempt was made.
    CacheHit,
}

/// The classification of a failed call, mirroring the core's error taxonomy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObservedError {
    /// The error's class.
    pub class: ErrorClass,
    /// The HTTP status, when the class is [`ErrorClass::Http`].
    pub http_status: Option<u16>,
}

/// One intercepted call, as the engine observed it.
#[derive(Debug, Clone)]
pub struct CallObservation {
    /// The resolved target, e.g. `"api.stripe.com"` or `"py:pipeline.enrich"`.
    pub target: String,
    /// How it resolved.
    pub result: CallResult,
    /// Upstream attempts consumed (`0` for a cache hit, else `>= 1`).
    pub attempts: u32,
    /// End-to-end latency in milliseconds.
    pub latency_ms: i64,
    /// Whether the rate limiter delayed this call.
    pub throttled: bool,
    /// Whether this call saw the breaker open.
    pub breaker_opened: bool,
    /// Whether this call failed and was NOT retried because it is not
    /// idempotent (KEEL-E014) — the "observed, not retried" Level 0 rule.
    pub not_retried: bool,
    /// Whether an explicit `[target."…"]` policy entry applied to this call.
    /// `false` means the call ran on layered defaults (or nothing) — it counts
    /// toward the coverage gap `keel status` reports.
    pub wrapped: bool,
    /// The failure classification, when the call failed.
    pub error: Option<ObservedError>,
}

/// Per-target aggregates: the output of [`DiscoveryStore::snapshot`] and the
/// input to [`DiscoveryStore::merge_report`] (the same shape flows both ways,
/// so a report snapshotted in one process merges into another's store). Counts
/// are non-negative; `i64` matches SQLite's native integer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TargetStats {
    /// The target these stats describe.
    pub target: String,
    /// Intercepted calls.
    pub calls: i64,
    /// Upstream attempts summed across calls.
    pub attempts: i64,
    /// Attempts beyond the first (i.e. retries) summed across calls.
    pub retries: i64,
    /// Upstream successes.
    pub successes: i64,
    /// Terminal failures.
    pub failures: i64,
    /// Calls served from cache.
    pub cache_hits: i64,
    /// Calls the rate limiter delayed.
    pub throttled: i64,
    /// Calls that saw an open breaker.
    pub breaker_opens: i64,
    /// Summed latency, for computing a mean.
    pub total_latency_ms: i64,
    /// Worst observed latency.
    pub max_latency_ms: i64,
    /// First observation (ms since epoch).
    pub first_seen_ms: i64,
    /// Most recent observation (ms since epoch).
    pub last_seen_ms: i64,
    /// The most recent error's class, if any.
    pub last_error_class: Option<ErrorClass>,
    /// The most recent error's HTTP status, if any.
    pub last_error_status: Option<u16>,
    /// Calls that failed and were not retried because non-idempotent (KEEL-E014).
    pub not_retried: i64,
    /// Calls that ran with no explicit `[target."…"]` policy entry.
    pub unwrapped_calls: i64,
}

/// One `(target, day)` bucket of the rolling daily aggregates — the output of
/// [`DiscoveryStore::daily_snapshot`], from which time-windowed answers
/// ("retries saved this week") are computed on *stored* days.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DailyStats {
    /// The target this bucket describes.
    pub target: String,
    /// UTC day index: `ms_since_epoch / 86_400_000`.
    pub day: i64,
    /// Intercepted calls.
    pub calls: i64,
    /// Upstream attempts summed across calls.
    pub attempts: i64,
    /// Attempts beyond the first summed across calls.
    pub retries: i64,
    /// Upstream successes.
    pub successes: i64,
    /// Terminal failures.
    pub failures: i64,
    /// Calls served from cache.
    pub cache_hits: i64,
    /// Calls the rate limiter delayed.
    pub throttled: i64,
    /// Calls that saw an open breaker.
    pub breaker_opens: i64,
    /// Calls that failed and were not retried because non-idempotent (KEEL-E014).
    pub not_retried: i64,
    /// Calls that ran with no explicit `[target."…"]` policy entry.
    pub unwrapped_calls: i64,
}

/// One `discovery` row's worth of values to feed the [`UPSERT`], borrowing its
/// string fields so neither caller has to clone.
struct AggregateRow<'a> {
    target: &'a str,
    calls: i64,
    attempts: i64,
    retries: i64,
    successes: i64,
    failures: i64,
    cache_hits: i64,
    throttled: i64,
    breaker_opens: i64,
    total_latency_ms: i64,
    max_latency_ms: i64,
    first_seen_ms: i64,
    last_seen_ms: i64,
    last_error_class: Option<&'static str>,
    last_error_status: Option<i64>,
    not_retried: i64,
    unwrapped_calls: i64,
}

/// A per-target traffic ledger over its own WAL-mode SQLite file, generic over
/// the [`Clock`] that stamps `first_seen`/`last_seen` (and keys daily buckets).
pub struct DiscoveryStore<C: Clock> {
    conn: Mutex<Connection>,
    clock: C,
    /// The UTC day retention was last enforced for (`-1` = never): the prune
    /// DELETE runs only when the bucket day advances, keeping the per-call
    /// write path at its usual two UPSERTs.
    last_prune_day: AtomicI64,
}

impl<C: Clock> core::fmt::Debug for DiscoveryStore<C> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DiscoveryStore")
            .field("clock", &self.clock)
            .finish_non_exhaustive()
    }
}

impl<C: Clock> DiscoveryStore<C> {
    /// Open (creating if absent) the discovery store at `path`. Convention is
    /// `.keel/discovery.db`, but the path is the caller's to choose. A legacy
    /// (v1) file is migrated in place — see the module docs.
    pub fn open(path: impl AsRef<Path>, clock: C) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(CONNECTION_PRAGMAS)?;
        migrate(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            clock,
            last_prune_day: AtomicI64::new(-1),
        })
    }

    /// Open the store **read-only** for the inspection commands (`keel status`,
    /// `keel init`, `keel doctor`): no `CREATE TABLE`, no WAL/synchronous
    /// pragmas, no write lock, and — deliberately — no migration. This lets
    /// those nominally read-only commands grade evidence from a read-only
    /// checkout or mounted volume, and never mutate file state (mirrors `keel
    /// flows`/`trace`, which open the journal `SQLITE_OPEN_READ_ONLY`). On a
    /// legacy (v1) file the reads degrade gracefully: [`Self::snapshot`] fills
    /// the v2 counters with zero and [`Self::daily_snapshot`] is empty.
    /// `clock` is inert on a read path.
    pub fn open_readonly(path: impl AsRef<Path>, clock: C) -> Result<Self> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        Ok(Self {
            conn: Mutex::new(conn),
            clock,
            last_prune_day: AtomicI64::new(-1),
        })
    }

    /// Fold one observed call into its target's aggregates (lifetime row plus
    /// the clock-day bucket), enforcing bucket retention when the day advances.
    pub fn record(&self, obs: &CallObservation) -> Result<()> {
        let now = self.clock.now_ms();
        let attempts = i64::from(obs.attempts);
        let row = AggregateRow {
            target: &obs.target,
            calls: 1,
            attempts,
            retries: i64::from(obs.attempts.saturating_sub(1)),
            successes: i64::from(obs.result == CallResult::Success),
            failures: i64::from(obs.result == CallResult::Failure),
            cache_hits: i64::from(obs.result == CallResult::CacheHit),
            throttled: i64::from(obs.throttled),
            breaker_opens: i64::from(obs.breaker_opened),
            total_latency_ms: obs.latency_ms,
            max_latency_ms: obs.latency_ms,
            first_seen_ms: now,
            last_seen_ms: now,
            last_error_class: obs.error.map(|e| error_class_str(e.class)),
            last_error_status: obs.error.and_then(|e| e.http_status).map(i64::from),
            not_retried: i64::from(obs.not_retried),
            unwrapped_calls: i64::from(!obs.wrapped),
        };
        let day = now.div_euclid(MS_PER_DAY);
        let conn = self.lock();
        upsert(&conn, &row)?;
        upsert_daily(&conn, &row, day)?;
        self.prune(&conn, day)
    }

    /// Merge a batch of already-aggregated stats (e.g. an in-process report
    /// snapshotted elsewhere) into this store, adding counters onto whatever is
    /// present. Each entry's daily bucket is keyed by its `last_seen_ms` day —
    /// the finest attribution an aggregate can honestly claim.
    pub fn merge_report(&self, report: &[TargetStats]) -> Result<()> {
        let clock_day = self.clock.now_ms().div_euclid(MS_PER_DAY);
        let conn = self.lock();
        for stats in report {
            let row = AggregateRow {
                target: &stats.target,
                calls: stats.calls,
                attempts: stats.attempts,
                retries: stats.retries,
                successes: stats.successes,
                failures: stats.failures,
                cache_hits: stats.cache_hits,
                throttled: stats.throttled,
                breaker_opens: stats.breaker_opens,
                total_latency_ms: stats.total_latency_ms,
                max_latency_ms: stats.max_latency_ms,
                first_seen_ms: stats.first_seen_ms,
                last_seen_ms: stats.last_seen_ms,
                last_error_class: stats.last_error_class.map(error_class_str),
                last_error_status: stats.last_error_status.map(i64::from),
                not_retried: stats.not_retried,
                unwrapped_calls: stats.unwrapped_calls,
            };
            upsert(&conn, &row)?;
            upsert_daily(&conn, &row, stats.last_seen_ms.div_euclid(MS_PER_DAY))?;
        }
        self.prune(&conn, clock_day)
    }

    /// All target aggregates, ordered by target — deterministic, so a snapshot
    /// diffs cleanly across runs. On a legacy (v1) file the v2 counters read
    /// as zero.
    pub fn snapshot(&self) -> Result<Vec<TargetStats>> {
        let conn = self.lock();
        let legacy = schema_version(&conn)? < DISCOVERY_SCHEMA_VERSION;
        let sql = if legacy {
            "SELECT target, calls, attempts, retries, successes, failures, cache_hits, \
             throttled, breaker_opens, total_latency_ms, max_latency_ms, first_seen_ms, \
             last_seen_ms, last_error_class, last_error_status, 0, 0 \
             FROM discovery ORDER BY target"
        } else {
            "SELECT target, calls, attempts, retries, successes, failures, cache_hits, \
             throttled, breaker_opens, total_latency_ms, max_latency_ms, first_seen_ms, \
             last_seen_ms, last_error_class, last_error_status, not_retried, \
             unwrapped_calls FROM discovery ORDER BY target"
        };
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt
            .query_map([], stats_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter().map(stats_from_row).collect()
    }

    /// All rolling daily buckets, ordered by (target, day) — deterministic.
    /// Empty on a legacy (v1) file, which has no bucket table.
    pub fn daily_snapshot(&self) -> Result<Vec<DailyStats>> {
        let conn = self.lock();
        if schema_version(&conn)? < DISCOVERY_SCHEMA_VERSION {
            return Ok(Vec::new());
        }
        let mut stmt = conn.prepare(
            "SELECT target, day, calls, attempts, retries, successes, failures, \
             cache_hits, throttled, breaker_opens, not_retried, unwrapped_calls \
             FROM discovery_daily ORDER BY target, day",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(DailyStats {
                    target: row.get(0)?,
                    day: row.get(1)?,
                    calls: row.get(2)?,
                    attempts: row.get(3)?,
                    retries: row.get(4)?,
                    successes: row.get(5)?,
                    failures: row.get(6)?,
                    cache_hits: row.get(7)?,
                    throttled: row.get(8)?,
                    breaker_opens: row.get(9)?,
                    not_retried: row.get(10)?,
                    unwrapped_calls: row.get(11)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Drop daily buckets that fell out of the retention window, at most once
    /// per advanced day (the atomic keeps the hot path at two UPSERTs).
    fn prune(&self, conn: &Connection, day: i64) -> Result<()> {
        if self.last_prune_day.swap(day, Ordering::Relaxed) == day {
            return Ok(());
        }
        conn.execute(
            "DELETE FROM discovery_daily WHERE day < ?1",
            params![day - (RETENTION_DAYS - 1)],
        )?;
        Ok(())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn
            .lock()
            .expect("discovery connection mutex poisoned")
    }
}

/// Read the schema version stamp (0 on a legacy v1 file).
fn schema_version(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("PRAGMA user_version", [], |r| r.get(0))?)
}

/// Bring a read-write connection to [`DISCOVERY_SCHEMA_VERSION`]: create the
/// current schema on a fresh file, or append the v2 counter columns and the
/// daily table to a legacy one. `BEGIN IMMEDIATE` + a re-check serialize two
/// processes migrating the same file (the loser sees the stamped version and
/// does nothing).
fn migrate(conn: &Connection) -> Result<()> {
    if schema_version(conn)? >= DISCOVERY_SCHEMA_VERSION {
        return Ok(());
    }
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let outcome = migrate_locked(conn);
    match outcome {
        Ok(()) => {
            conn.execute_batch("COMMIT")?;
            Ok(())
        }
        Err(e) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

fn migrate_locked(conn: &Connection) -> Result<()> {
    if schema_version(conn)? >= DISCOVERY_SCHEMA_VERSION {
        return Ok(()); // another process won the race
    }
    let has_table: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'discovery')",
        [],
        |r| r.get(0),
    )?;
    if has_table {
        let has_column: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('discovery') \
             WHERE name = 'not_retried')",
            [],
            |r| r.get(0),
        )?;
        if !has_column {
            // Appended, so a migrated file's column order matches a fresh v2 one.
            conn.execute_batch(
                "ALTER TABLE discovery ADD COLUMN not_retried INTEGER NOT NULL DEFAULT 0;
                 ALTER TABLE discovery ADD COLUMN unwrapped_calls INTEGER NOT NULL DEFAULT 0;",
            )?;
        }
    } else {
        conn.execute_batch(DISCOVERY_SCHEMA)?;
    }
    conn.execute_batch(DAILY_SCHEMA)?;
    conn.execute_batch("PRAGMA user_version = 2")?;
    Ok(())
}

fn upsert(conn: &Connection, row: &AggregateRow<'_>) -> Result<()> {
    conn.execute(
        UPSERT,
        params![
            row.target,
            row.calls,
            row.attempts,
            row.retries,
            row.successes,
            row.failures,
            row.cache_hits,
            row.throttled,
            row.breaker_opens,
            row.total_latency_ms,
            row.max_latency_ms,
            row.first_seen_ms,
            row.last_seen_ms,
            row.last_error_class,
            row.last_error_status,
            row.not_retried,
            row.unwrapped_calls,
        ],
    )?;
    Ok(())
}

fn upsert_daily(conn: &Connection, row: &AggregateRow<'_>, day: i64) -> Result<()> {
    conn.execute(
        DAILY_UPSERT,
        params![
            row.target,
            day,
            row.calls,
            row.attempts,
            row.retries,
            row.successes,
            row.failures,
            row.cache_hits,
            row.throttled,
            row.breaker_opens,
            row.not_retried,
            row.unwrapped_calls,
        ],
    )?;
    Ok(())
}

/// The raw `discovery` columns before typing.
struct StatsRowData {
    target: String,
    calls: i64,
    attempts: i64,
    retries: i64,
    successes: i64,
    failures: i64,
    cache_hits: i64,
    throttled: i64,
    breaker_opens: i64,
    total_latency_ms: i64,
    max_latency_ms: i64,
    first_seen_ms: i64,
    last_seen_ms: i64,
    last_error_class: Option<String>,
    last_error_status: Option<i64>,
    not_retried: i64,
    unwrapped_calls: i64,
}

fn stats_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StatsRowData> {
    Ok(StatsRowData {
        target: row.get(0)?,
        calls: row.get(1)?,
        attempts: row.get(2)?,
        retries: row.get(3)?,
        successes: row.get(4)?,
        failures: row.get(5)?,
        cache_hits: row.get(6)?,
        throttled: row.get(7)?,
        breaker_opens: row.get(8)?,
        total_latency_ms: row.get(9)?,
        max_latency_ms: row.get(10)?,
        first_seen_ms: row.get(11)?,
        last_seen_ms: row.get(12)?,
        last_error_class: row.get(13)?,
        last_error_status: row.get(14)?,
        not_retried: row.get(15)?,
        unwrapped_calls: row.get(16)?,
    })
}

fn stats_from_row(raw: StatsRowData) -> Result<TargetStats> {
    let last_error_class = raw
        .last_error_class
        .as_deref()
        .map(|value| error_class_from_db("discovery.last_error_class", value))
        .transpose()?;
    let last_error_status = raw
        .last_error_status
        .map(|s| u16::try_from(s).map_err(|_| Error::corrupt("discovery.last_error_status", s)))
        .transpose()?;
    Ok(TargetStats {
        target: raw.target,
        calls: raw.calls,
        attempts: raw.attempts,
        retries: raw.retries,
        successes: raw.successes,
        failures: raw.failures,
        cache_hits: raw.cache_hits,
        throttled: raw.throttled,
        breaker_opens: raw.breaker_opens,
        total_latency_ms: raw.total_latency_ms,
        max_latency_ms: raw.max_latency_ms,
        first_seen_ms: raw.first_seen_ms,
        last_seen_ms: raw.last_seen_ms,
        last_error_class,
        last_error_status,
        not_retried: raw.not_retried,
        unwrapped_calls: raw.unwrapped_calls,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::ManualClock;
    use tempfile::TempDir;

    const T0: i64 = 1_783_728_000_000;
    const T0_DAY: i64 = T0 / MS_PER_DAY;

    /// The v1 schema exactly as shipped before the versioned migration, for
    /// building legacy fixture files.
    const V1_SCHEMA: &str = "\
CREATE TABLE IF NOT EXISTS discovery (
    target            TEXT PRIMARY KEY,
    calls             INTEGER NOT NULL DEFAULT 0,
    attempts          INTEGER NOT NULL DEFAULT 0,
    retries           INTEGER NOT NULL DEFAULT 0,
    successes         INTEGER NOT NULL DEFAULT 0,
    failures          INTEGER NOT NULL DEFAULT 0,
    cache_hits        INTEGER NOT NULL DEFAULT 0,
    throttled         INTEGER NOT NULL DEFAULT 0,
    breaker_opens     INTEGER NOT NULL DEFAULT 0,
    total_latency_ms  INTEGER NOT NULL DEFAULT 0,
    max_latency_ms    INTEGER NOT NULL DEFAULT 0,
    first_seen_ms     INTEGER NOT NULL,
    last_seen_ms      INTEGER NOT NULL,
    last_error_class  TEXT,
    last_error_status INTEGER
) WITHOUT ROWID;";

    fn store(dir: &TempDir, clock: ManualClock) -> DiscoveryStore<ManualClock> {
        DiscoveryStore::open(dir.path().join("discovery.db"), clock).expect("open discovery")
    }

    /// Write a legacy (v1) discovery.db with one populated row.
    fn build_v1_fixture(dir: &TempDir) -> std::path::PathBuf {
        let path = dir.path().join("discovery.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(V1_SCHEMA).unwrap();
        conn.execute(
            "INSERT INTO discovery VALUES ('api.old', 10, 12, 2, 8, 2, 0, 1, 0, 500, 90, ?1, ?2, 'http', 503)",
            params![T0, T0 + 1_000],
        )
        .unwrap();
        drop(conn);
        path
    }

    fn success(target: &str, attempts: u32, latency_ms: i64) -> CallObservation {
        CallObservation {
            target: target.to_owned(),
            result: CallResult::Success,
            attempts,
            latency_ms,
            throttled: false,
            breaker_opened: false,
            not_retried: false,
            wrapped: true,
            error: None,
        }
    }

    fn zero_stats(target: &str) -> TargetStats {
        TargetStats {
            target: target.to_owned(),
            calls: 0,
            attempts: 0,
            retries: 0,
            successes: 0,
            failures: 0,
            cache_hits: 0,
            throttled: 0,
            breaker_opens: 0,
            total_latency_ms: 0,
            max_latency_ms: 0,
            first_seen_ms: T0,
            last_seen_ms: T0,
            last_error_class: None,
            last_error_status: None,
            not_retried: 0,
            unwrapped_calls: 0,
        }
    }

    #[test]
    fn record_accumulates_across_calls_for_one_target() {
        let dir = TempDir::new().unwrap();
        let clock = ManualClock::new(T0);
        let s = store(&dir, clock.clone());

        s.record(&success("api.x", 1, 100)).unwrap();
        clock.advance(1_000);
        s.record(&success("api.x", 3, 300)).unwrap(); // two retries

        let snap = s.snapshot().unwrap();
        assert_eq!(snap.len(), 1);
        let t = &snap[0];
        assert_eq!(t.calls, 2);
        assert_eq!(t.attempts, 4);
        assert_eq!(t.retries, 2);
        assert_eq!(t.successes, 2);
        assert_eq!(t.total_latency_ms, 400);
        assert_eq!(t.max_latency_ms, 300);
        assert_eq!(t.first_seen_ms, T0);
        assert_eq!(t.last_seen_ms, T0 + 1_000);
        assert_eq!(t.not_retried, 0);
        assert_eq!(t.unwrapped_calls, 0);
    }

    #[test]
    fn cache_hit_counts_as_a_call_but_not_a_success_or_attempt() {
        let dir = TempDir::new().unwrap();
        let s = store(&dir, ManualClock::new(T0));
        s.record(&CallObservation {
            target: "api.x".to_owned(),
            result: CallResult::CacheHit,
            attempts: 0,
            latency_ms: 2,
            throttled: false,
            breaker_opened: false,
            not_retried: false,
            wrapped: true,
            error: None,
        })
        .unwrap();

        let t = &s.snapshot().unwrap()[0];
        assert_eq!(
            (t.calls, t.cache_hits, t.successes, t.attempts),
            (1, 1, 0, 0)
        );
    }

    #[test]
    fn failure_records_its_error_class_and_status() {
        let dir = TempDir::new().unwrap();
        let clock = ManualClock::new(T0);
        let s = store(&dir, clock.clone());

        s.record(&CallObservation {
            target: "api.x".to_owned(),
            result: CallResult::Failure,
            attempts: 5,
            latency_ms: 50,
            throttled: true,
            breaker_opened: true,
            not_retried: false,
            wrapped: true,
            error: Some(ObservedError {
                class: ErrorClass::Http,
                http_status: Some(503),
            }),
        })
        .unwrap();

        let t = &s.snapshot().unwrap()[0];
        assert_eq!(t.failures, 1);
        assert_eq!(t.throttled, 1);
        assert_eq!(t.breaker_opens, 1);
        assert_eq!(t.last_error_class, Some(ErrorClass::Http));
        assert_eq!(t.last_error_status, Some(503));

        // A later success must not erase the last error.
        clock.advance(1_000);
        s.record(&success("api.x", 1, 10)).unwrap();
        let t = &s.snapshot().unwrap()[0];
        assert_eq!(t.last_error_class, Some(ErrorClass::Http));
        assert_eq!(t.last_error_status, Some(503));
    }

    #[test]
    fn not_retried_and_unwrapped_calls_accumulate() {
        let dir = TempDir::new().unwrap();
        let s = store(&dir, ManualClock::new(T0));

        // A non-idempotent failure: observed, not retried (KEEL-E014).
        s.record(&CallObservation {
            target: "api.x".to_owned(),
            result: CallResult::Failure,
            attempts: 1,
            latency_ms: 20,
            throttled: false,
            breaker_opened: false,
            not_retried: true,
            wrapped: true,
            error: Some(ObservedError {
                class: ErrorClass::Http,
                http_status: Some(500),
            }),
        })
        .unwrap();
        // A call on a target no [target] entry covers.
        s.record(&CallObservation {
            wrapped: false,
            ..success("api.x", 1, 10)
        })
        .unwrap();

        let t = &s.snapshot().unwrap()[0];
        assert_eq!(t.not_retried, 1);
        assert_eq!(t.unwrapped_calls, 1);
        assert_eq!(t.calls, 2);

        let daily = s.daily_snapshot().unwrap();
        assert_eq!(daily.len(), 1);
        assert_eq!(daily[0].day, T0_DAY);
        assert_eq!(daily[0].not_retried, 1);
        assert_eq!(daily[0].unwrapped_calls, 1);
    }

    #[test]
    fn snapshot_is_sorted_by_target_regardless_of_insert_order() {
        let dir = TempDir::new().unwrap();
        let s = store(&dir, ManualClock::new(T0));
        for target in ["api.zebra", "api.alpha", "api.mango"] {
            s.record(&success(target, 1, 1)).unwrap();
        }
        let order: Vec<_> = s
            .snapshot()
            .unwrap()
            .into_iter()
            .map(|t| t.target)
            .collect();
        assert_eq!(order, ["api.alpha", "api.mango", "api.zebra"]);
    }

    #[test]
    fn merge_report_adds_onto_existing_aggregates() {
        let dir = TempDir::new().unwrap();
        let clock = ManualClock::new(T0);
        let live = store(&dir, clock.clone());
        live.record(&success("api.x", 2, 100)).unwrap();

        // A report from "another process", carrying its own window.
        let report = vec![TargetStats {
            calls: 3,
            attempts: 3,
            successes: 3,
            total_latency_ms: 90,
            max_latency_ms: 40,
            first_seen_ms: T0 - 5_000,
            last_seen_ms: T0 + 9_000,
            not_retried: 2,
            unwrapped_calls: 1,
            ..zero_stats("api.x")
        }];
        live.merge_report(&report).unwrap();

        let t = &live.snapshot().unwrap()[0];
        assert_eq!(t.calls, 4);
        assert_eq!(t.attempts, 5);
        assert_eq!(t.successes, 4);
        assert_eq!(t.total_latency_ms, 190);
        assert_eq!(t.max_latency_ms, 100);
        assert_eq!(t.first_seen_ms, T0 - 5_000, "min across sources");
        assert_eq!(t.last_seen_ms, T0 + 9_000, "max across sources");
        assert_eq!(t.not_retried, 2);
        assert_eq!(t.unwrapped_calls, 1);
    }

    #[test]
    fn daily_buckets_split_by_utc_day_and_merge_keys_on_last_seen() {
        let dir = TempDir::new().unwrap();
        let clock = ManualClock::new(T0);
        let s = store(&dir, clock.clone());

        s.record(&success("api.x", 2, 10)).unwrap(); // day T0_DAY, 1 retry
        clock.advance(MS_PER_DAY); // next UTC day
        s.record(&success("api.x", 3, 10)).unwrap(); // day T0_DAY+1, 2 retries

        // An aggregate merged with last_seen two days later buckets there.
        s.merge_report(&[TargetStats {
            calls: 5,
            attempts: 9,
            retries: 4,
            successes: 5,
            first_seen_ms: T0,
            last_seen_ms: T0 + 3 * MS_PER_DAY,
            ..zero_stats("api.x")
        }])
        .unwrap();

        let daily = s.daily_snapshot().unwrap();
        let days: Vec<(i64, i64)> = daily.iter().map(|d| (d.day, d.retries)).collect();
        assert_eq!(
            days,
            [(T0_DAY, 1), (T0_DAY + 1, 2), (T0_DAY + 3, 4)],
            "one bucket per stored day, sorted"
        );
        // Lifetime totals still hold the sum.
        assert_eq!(s.snapshot().unwrap()[0].retries, 7);
    }

    #[test]
    fn buckets_older_than_retention_are_pruned_when_the_day_advances() {
        let dir = TempDir::new().unwrap();
        let clock = ManualClock::new(T0);
        let s = store(&dir, clock.clone());
        s.record(&success("api.x", 1, 1)).unwrap(); // bucket at T0_DAY

        clock.advance((RETENTION_DAYS + 5) * MS_PER_DAY);
        s.record(&success("api.x", 1, 1)).unwrap(); // day advance triggers prune

        let daily = s.daily_snapshot().unwrap();
        assert_eq!(daily.len(), 1, "the stale bucket was pruned");
        assert_eq!(daily[0].day, T0_DAY + RETENTION_DAYS + 5);
        // The lifetime row is untouched by retention.
        assert_eq!(s.snapshot().unwrap()[0].calls, 2);
    }

    #[test]
    fn v1_file_is_migrated_in_place_preserving_rows() {
        let dir = TempDir::new().unwrap();
        let path = build_v1_fixture(&dir);

        // Opening read-write migrates: version stamped, columns appended,
        // daily table present, old counters intact and new ones zero.
        let s = DiscoveryStore::open(&path, ManualClock::new(T0)).unwrap();
        {
            let conn = s.lock();
            assert_eq!(schema_version(&conn).unwrap(), DISCOVERY_SCHEMA_VERSION);
        }
        let t = &s.snapshot().unwrap()[0];
        assert_eq!(t.target, "api.old");
        assert_eq!(t.calls, 10);
        assert_eq!(t.last_error_status, Some(503));
        assert_eq!((t.not_retried, t.unwrapped_calls), (0, 0));
        assert_eq!(s.daily_snapshot().unwrap(), []);

        // And the migrated file records v2 observations normally.
        s.record(&CallObservation {
            not_retried: true,
            wrapped: false,
            ..success("api.old", 1, 5)
        })
        .unwrap();
        let t = &s.snapshot().unwrap()[0];
        assert_eq!((t.calls, t.not_retried, t.unwrapped_calls), (11, 1, 1));
        assert_eq!(s.daily_snapshot().unwrap().len(), 1);
    }

    #[test]
    fn readonly_open_of_v1_file_degrades_gracefully_without_migrating() {
        let dir = TempDir::new().unwrap();
        let path = build_v1_fixture(&dir);

        let s = DiscoveryStore::open_readonly(&path, ManualClock::new(T0)).unwrap();
        let t = &s.snapshot().unwrap()[0];
        assert_eq!(t.calls, 10);
        assert_eq!((t.not_retried, t.unwrapped_calls), (0, 0));
        assert_eq!(s.daily_snapshot().unwrap(), []);
        drop(s);

        // The file was not mutated: still v1.
        let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        assert_eq!(schema_version(&conn).unwrap(), 0);
    }

    #[test]
    fn migration_is_idempotent_across_reopens() {
        let dir = TempDir::new().unwrap();
        let path = build_v1_fixture(&dir);
        for _ in 0..2 {
            let s = DiscoveryStore::open(&path, ManualClock::new(T0)).unwrap();
            assert_eq!(s.snapshot().unwrap()[0].calls, 10);
        }
        // Column count is exactly the v2 shape (no duplicate ALTERs).
        let conn = Connection::open(&path).unwrap();
        let cols: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('discovery')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cols, 17);
    }
}
