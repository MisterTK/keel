//! The discovery store: the third evidence source behind `keel init`,
//! `keel status`, and `keel doctor` (DX spec §2).
//!
//! Where the journal records *flows*, this records *traffic*: per-target
//! running aggregates cheap enough to update on every intercepted call, so a
//! session accumulates the evidence needed to propose (or grade) a policy.
//! Its schema is **not** contract-frozen — this module owns it, and here it is:
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
//!     last_error_status INTEGER                      -- …and its HTTP status
//! ) WITHOUT ROWID;
//! ```
//!
//! Accounting: a cache hit is a `call` and a `cache_hit` only — it consumed no
//! upstream attempt, so it is neither a `success` nor a `failure`; thus
//! `calls == successes + failures + cache_hits`. `last_error_*` tracks the most
//! recent error and assumes records/merges arrive roughly time-ordered (they
//! do: `last_seen_ms` is monotonic under a forward-moving clock).
//!
//! Every mutation is a single UPSERT, so two processes recording into one file
//! accumulate correctly without a transaction.

use std::path::Path;
use std::sync::Mutex;

use keel_core_api::ErrorClass;
use rusqlite::{Connection, OpenFlags, params};
use serde::Serialize;

use crate::clock::Clock;
use crate::error::{Error, Result};
use crate::types::{error_class_from_db, error_class_str};

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
    last_error_status INTEGER
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
     first_seen_ms, last_seen_ms, last_error_class, last_error_status)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
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
        ELSE last_error_status END";

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
}

/// A per-target traffic ledger over its own WAL-mode SQLite file, generic over
/// the [`Clock`] that stamps `first_seen`/`last_seen`.
pub struct DiscoveryStore<C: Clock> {
    conn: Mutex<Connection>,
    clock: C,
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
    /// `.keel/discovery.db`, but the path is the caller's to choose.
    pub fn open(path: impl AsRef<Path>, clock: C) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(CONNECTION_PRAGMAS)?;
        conn.execute_batch(DISCOVERY_SCHEMA)?;
        Ok(Self {
            conn: Mutex::new(conn),
            clock,
        })
    }

    /// Open the store **read-only** for the inspection commands (`keel status`,
    /// `keel init`, `keel doctor`): no `CREATE TABLE`, no WAL/synchronous
    /// pragmas, no write lock. This lets those nominally read-only commands grade
    /// evidence from a read-only checkout or mounted volume, and never mutate
    /// file state (mirrors `keel flows`/`trace`, which open the journal
    /// `SQLITE_OPEN_READ_ONLY`). `clock` is inert on a read path.
    pub fn open_readonly(path: impl AsRef<Path>, clock: C) -> Result<Self> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        Ok(Self {
            conn: Mutex::new(conn),
            clock,
        })
    }

    /// Fold one observed call into its target's aggregates.
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
        };
        let conn = self.lock();
        upsert(&conn, &row)
    }

    /// Merge a batch of already-aggregated stats (e.g. an in-process report
    /// snapshotted elsewhere) into this store, adding counters onto whatever is
    /// present.
    pub fn merge_report(&self, report: &[TargetStats]) -> Result<()> {
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
            };
            upsert(&conn, &row)?;
        }
        Ok(())
    }

    /// All target aggregates, ordered by target — deterministic, so a snapshot
    /// diffs cleanly across runs.
    pub fn snapshot(&self) -> Result<Vec<TargetStats>> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT target, calls, attempts, retries, successes, failures, cache_hits, \
             throttled, breaker_opens, total_latency_ms, max_latency_ms, first_seen_ms, \
             last_seen_ms, last_error_class, last_error_status \
             FROM discovery ORDER BY target",
        )?;
        let rows = stmt
            .query_map([], stats_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter().map(stats_from_row).collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn
            .lock()
            .expect("discovery connection mutex poisoned")
    }
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::ManualClock;
    use tempfile::TempDir;

    const T0: i64 = 1_783_728_000_000;

    fn store(dir: &TempDir, clock: ManualClock) -> DiscoveryStore<ManualClock> {
        DiscoveryStore::open(dir.path().join("discovery.db"), clock).expect("open discovery")
    }

    fn success(target: &str, attempts: u32, latency_ms: i64) -> CallObservation {
        CallObservation {
            target: target.to_owned(),
            result: CallResult::Success,
            attempts,
            latency_ms,
            throttled: false,
            breaker_opened: false,
            error: None,
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
            target: "api.x".to_owned(),
            calls: 3,
            attempts: 3,
            retries: 0,
            successes: 3,
            failures: 0,
            cache_hits: 0,
            throttled: 0,
            breaker_opens: 0,
            total_latency_ms: 90,
            max_latency_ms: 40,
            first_seen_ms: T0 - 5_000,
            last_seen_ms: T0 + 9_000,
            last_error_class: None,
            last_error_status: None,
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
    }
}
