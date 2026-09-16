/**
 * Discovery store: the always-on, cheap per-target traffic ledger written to
 * `.keel/discovery.db` (DX spec §2 — the evidence that makes `keel init`
 * evidence-based). SQLite with ZERO native deps via Node's builtin `node:sqlite`
 * (`DatabaseSync`; floor node>=22.5, declared in engines).
 *
 * The `discovery` (+ `discovery_daily`) tables match the CANONICAL schema owned
 * by `crates/keel-journal` (`src/discovery.rs`) exactly, so one
 * `.keel/discovery.db` is inspectable the same way whether it was written by
 * the Rust core, the Python front end, or this one: one WITHOUT ROWID row per
 * target, WAL mode, and a single self-contained UPSERT per table that
 * accumulates counters and keeps the latency/seen extremes (`max`/`min`) and
 * the most-recent error.
 *
 * Accounting mirrors the canonical store: a cache hit is a `call` and a
 * `cache_hit` only — it consumed no upstream attempt, so it is neither a
 * `success` nor a `failure`; thus `calls == successes + failures + cache_hits`.
 * `breaker_opens` counts calls that saw an OPEN breaker (fail-fast, KEEL-E012),
 * not breaker transitions. `last_error_*` tracks the most recent failure and is
 * never erased by a later success (coalesce in the UPSERT). `not_retried`
 * counts calls that resolved KEEL-E014 (failed, and Keel refused to retry
 * because the call is not idempotent — the DX Level 0 hard rule's "observed,
 * not retried"). `unwrapped_calls` counts calls whose target had no explicit
 * `[target."…"]` entry in the effective policy handed to `backend.configure()`
 * — the coverage gap `keel status` reports; `knownTargets` (that policy's
 * `target` keys) is supplied once at construction by `bootstrap.mjs`.
 *
 * Observations are folded into per-target aggregates in memory on the hot path
 * (cheap: integer arithmetic + one injected-clock read) and written once at
 * flush (lifetime row plus the clock-day bucket, with retention pruning when
 * the bucket day advances), so discovery never throws into, slows, or adds
 * output to the user's program (DX invariant 4). `node:sqlite` is loaded
 * lazily and synchronously (createRequire) at flush only, so it never loads
 * when Keel is disabled or when nothing was observed. The clock is injectable
 * for deterministic tests.
 *
 * Migration: a file written by the previous (v1, `user_version = 0`) schema is
 * upgraded in place at flush — the two counter columns are appended
 * (`ALTER TABLE … ADD COLUMN`), the daily table is created, and
 * `user_version` is stamped to 2. A v2 file gains the `activations` table
 * (one row per process, WS8/#92) and is stamped to 3. Mirrors
 * `keel_journal::discovery::migrate` exactly, so either writer can open a file
 * the other created.
 *
 * `recordActivation(row)` only remembers the row; it is written once, lazily,
 * at the next `flushSync()` (Node's only write path — aggregates are buffered
 * in memory and written once too), so an activated-but-idle process still
 * leaves exactly one row of evidence at exit. Retention keeps the newest 50
 * rows (by `ts_ms` then `rowid`), mirroring the crate and the Python twin.
 */

import { createRequire } from "node:module";
import { mkdirSync } from "node:fs";
import { dirname, join } from "node:path";

/** Current discovery schema version, stamped in `PRAGMA user_version`. */
export const SCHEMA_VERSION = 3;

/** How many trailing UTC days of `discovery_daily` buckets are kept. */
export const RETENTION_DAYS = 30;

/** Milliseconds per UTC day; `day = Math.floor(ms / MS_PER_DAY)` is the bucket key. */
export const MS_PER_DAY = 86_400_000;

export function createDiscovery(
  cwd = process.cwd(),
  { now = Date.now, knownTargets = new Set(), summary = null, cachepoll = null } = {}
) {
  const dbPath = join(cwd, ".keel", "discovery.db");
  const aggregates = new Map(); // target -> Aggregate
  let activation = null;
  let activationWritten = false;

  return {
    dbPath,

    /**
     * Remember this process's activation (WS8/#92). Written lazily, at the
     * next `flushSync()` — so an idle-but-activated process still leaves
     * exactly one row of evidence at exit, and a never-activated process
     * never touches the filesystem.
     */
    recordActivation(row) {
      activation = { ...row };
    },

    /**
     * Hot-path: fold one intercepted call's Outcome envelope into its target's
     * aggregate. `latencyMs` is the call's end-to-end time (0 if unknown).
     * `argsHash` (WS9, #78) feeds the runtime cache-poll detector — absent
     * for call shapes with no args-hash concept.
     */
    observe(target, outcome, latencyMs = 0, argsHash = null) {
      if (!target || outcome == null) return;
      // The exit-time console summary (src/summary.mjs) is fed here because
      // this is the one place that knows whether the target was wrapped.
      try {
        summary?.observe(outcome, knownTargets.has(target), target);
      } catch {
        /* the summary never breaks a call */
      }
      try {
        if (cachepoll?.observe(target, argsHash, outcome)) summary?.noteCachePollSuspect();
      } catch {
        /* a detector bug must never break a call */
      }
      let a = aggregates.get(target);
      if (!a) aggregates.set(target, (a = newAggregate()));

      const attempts = Number.isFinite(outcome.attempts) ? outcome.attempts : 0;
      const cacheHit = outcome.from_cache === true;
      a.calls += 1;
      a.attempts += attempts;
      a.retries += attempts > 1 ? attempts - 1 : 0;
      if (cacheHit) a.cache_hits += 1;
      else if (outcome.result === "ok") a.successes += 1;
      else a.failures += 1;
      if (outcome.throttled) a.throttled += 1;
      if (outcome.error?.code === "KEEL-E012") a.breaker_opens += 1; // saw an open breaker
      if (outcome.error?.code === "KEEL-E014") a.not_retried += 1; // observed, not retried
      if (!knownTargets.has(target)) a.unwrapped_calls += 1;

      const lat = Number.isFinite(latencyMs) && latencyMs > 0 ? Math.round(latencyMs) : 0;
      a.total_latency_ms += lat;
      if (lat > a.max_latency_ms) a.max_latency_ms = lat;

      const nowMs = now();
      if (a.first_seen_ms === null) a.first_seen_ms = nowMs;
      a.last_seen_ms = nowMs;

      if (outcome.result === "error" && outcome.error) {
        a.last_error_class = outcome.error.class ?? null;
        a.last_error_status = Number.isInteger(outcome.error.http_status)
          ? outcome.error.http_status
          : null;
      }
    },

    /**
     * Persist accumulated aggregates via the canonical UPSERT (lifetime row +
     * daily bucket), migrating a legacy file in place first. Never throws.
     */
    flushSync() {
      if (aggregates.size === 0 && (activation === null || activationWritten)) return false;
      try {
        const require = createRequire(import.meta.url);
        const { DatabaseSync } = require("node:sqlite");
        mkdirSync(dirname(dbPath), { recursive: true });
        const db = new DatabaseSync(dbPath);
        try {
          db.exec(CONNECTION_PRAGMAS);
          migrate(db);
          if (activation !== null && !activationWritten) {
            db.prepare(ACTIVATION_INSERT).run(
              Number(activation.ts_ms), Number(activation.pid), String(activation.language), String(activation.version),
              String(activation.cwd), activation.keel_cwd ?? null, String(activation.policy_source),
              activation.policy_path ?? null, activation.flows_configured ? 1 : 0, String(activation.argv0 ?? ""),
            );
            db.prepare(ACTIVATION_PRUNE).run();
            activationWritten = true;
          }
          const upsert = db.prepare(UPSERT);
          const dailyUpsert = db.prepare(DAILY_UPSERT);
          let day = null;
          for (const [target, a] of aggregates) {
            const fallback = now();
            const lastSeen = a.last_seen_ms ?? fallback;
            upsert.run(
              target,
              a.calls,
              a.attempts,
              a.retries,
              a.successes,
              a.failures,
              a.cache_hits,
              a.throttled,
              a.breaker_opens,
              a.total_latency_ms,
              a.max_latency_ms,
              a.first_seen_ms ?? fallback,
              lastSeen,
              a.last_error_class,
              a.last_error_status,
              a.not_retried,
              a.unwrapped_calls
            );
            const bucketDay = Math.floor(lastSeen / MS_PER_DAY);
            day = day === null ? bucketDay : Math.max(day, bucketDay);
            dailyUpsert.run(
              target,
              bucketDay,
              a.calls,
              a.attempts,
              a.retries,
              a.successes,
              a.failures,
              a.cache_hits,
              a.throttled,
              a.breaker_opens,
              a.not_retried,
              a.unwrapped_calls
            );
          }
          if (day !== null) prune(db, day);
          return true;
        } finally {
          db.close();
        }
      } catch {
        return false; // best-effort: swallow (permissions, fs, etc.)
      }
    },
  };
}

function newAggregate() {
  return {
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
    first_seen_ms: null,
    last_seen_ms: null,
    last_error_class: null,
    last_error_status: null,
    not_retried: 0,
    unwrapped_calls: 0,
  };
}

/** Drop daily buckets older than [`RETENTION_DAYS`], as of `day`. */
function prune(db, day) {
  db.prepare("DELETE FROM discovery_daily WHERE day < ?").run(day - (RETENTION_DAYS - 1));
}

/**
 * Bring `db` to [`SCHEMA_VERSION`]: create the current schema on a fresh
 * file, or append the v2 counter columns and the daily table to a legacy
 * (v1) one. Mirrors `keel_journal::discovery::migrate` — idempotent, so
 * re-flushing to an already-migrated file is a no-op.
 */
function migrate(db) {
  const version = db.prepare("PRAGMA user_version").get().user_version;
  if (version >= SCHEMA_VERSION) return;
  if (version < 2) {
    const hasTable = Boolean(
      db
        .prepare(
          "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'discovery') AS x"
        )
        .get().x
    );
    if (hasTable) {
      const hasColumn = Boolean(
        db
          .prepare(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('discovery') WHERE name = 'not_retried') AS x"
          )
          .get().x
      );
      if (!hasColumn) {
        // Appended, so a migrated file's column order matches a fresh v2 one.
        db.exec("ALTER TABLE discovery ADD COLUMN not_retried INTEGER NOT NULL DEFAULT 0;");
        db.exec("ALTER TABLE discovery ADD COLUMN unwrapped_calls INTEGER NOT NULL DEFAULT 0;");
      }
    } else {
      db.exec(DISCOVERY_SCHEMA);
    }
    db.exec(DAILY_SCHEMA);
  }
  if (version < 3) db.exec(ACTIVATIONS_SCHEMA);
  db.exec(`PRAGMA user_version = ${SCHEMA_VERSION};`);
}

const CONNECTION_PRAGMAS = `
PRAGMA journal_mode = WAL;
PRAGMA busy_timeout = 5000;
PRAGMA synchronous = NORMAL;
`;

// Canonical schema — MUST match crates/keel-journal/src/discovery.rs verbatim.
// Exported for tests that need to seed a legacy (pre-activations) v2 file.
export const DISCOVERY_SCHEMA = `
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
) WITHOUT ROWID;
`;

// Canonical daily-bucket schema — MUST match discovery.rs's DAILY_SCHEMA.
export const DAILY_SCHEMA = `
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
) WITHOUT ROWID;
`;

// Single self-contained UPSERT — identical semantics to the canonical store:
// counters add; latency/seen keep max/min; the error columns move together,
// gated on the incoming row actually carrying an error (so a later success never
// erases the last error).
const UPSERT = `
INSERT INTO discovery
  (target, calls, attempts, retries, successes, failures, cache_hits, throttled,
   breaker_opens, total_latency_ms, max_latency_ms, first_seen_ms, last_seen_ms,
   last_error_class, last_error_status, not_retried, unwrapped_calls)
VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
ON CONFLICT(target) DO UPDATE SET
  calls             = calls + excluded.calls,
  attempts          = attempts + excluded.attempts,
  retries           = retries + excluded.retries,
  successes         = successes + excluded.successes,
  failures          = failures + excluded.failures,
  cache_hits        = cache_hits + excluded.cache_hits,
  throttled         = throttled + excluded.throttled,
  breaker_opens     = breaker_opens + excluded.breaker_opens,
  total_latency_ms  = total_latency_ms + excluded.total_latency_ms,
  max_latency_ms    = max(max_latency_ms, excluded.max_latency_ms),
  first_seen_ms     = min(first_seen_ms, excluded.first_seen_ms),
  last_seen_ms      = max(last_seen_ms, excluded.last_seen_ms),
  last_error_class  = coalesce(excluded.last_error_class, last_error_class),
  last_error_status = CASE
    WHEN excluded.last_error_class IS NOT NULL THEN excluded.last_error_status
    ELSE last_error_status END,
  not_retried       = not_retried + excluded.not_retried,
  unwrapped_calls   = unwrapped_calls + excluded.unwrapped_calls
`;

// The daily-bucket twin of UPSERT: pure counter addition per (target, day).
const DAILY_UPSERT = `
INSERT INTO discovery_daily
  (target, day, calls, attempts, retries, successes, failures, cache_hits,
   throttled, breaker_opens, not_retried, unwrapped_calls)
VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
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
  unwrapped_calls = unwrapped_calls + excluded.unwrapped_calls
`;

// Canonical activations schema — MUST match crates/keel-journal/src/discovery.rs
// and python/keel/src/keel/_discovery.py verbatim. One row per process (WS8/#92).
const ACTIVATIONS_SCHEMA = `
CREATE TABLE IF NOT EXISTS activations (
    ts_ms            INTEGER NOT NULL,
    pid              INTEGER NOT NULL,
    language         TEXT    NOT NULL,
    version          TEXT    NOT NULL,
    cwd              TEXT    NOT NULL,
    keel_cwd         TEXT,
    policy_source    TEXT    NOT NULL,
    policy_path      TEXT,
    flows_configured INTEGER NOT NULL DEFAULT 0,
    argv0            TEXT    NOT NULL DEFAULT ''
);
`;

const ACTIVATION_INSERT = `
INSERT INTO activations (ts_ms, pid, language, version, cwd, keel_cwd, policy_source,
  policy_path, flows_configured, argv0) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
`;

// Retention: keep the newest 50 rows, mirroring the crate and the Python twin.
const ACTIVATION_PRUNE = `
DELETE FROM activations WHERE rowid NOT IN
  (SELECT rowid FROM activations ORDER BY ts_ms DESC, rowid DESC LIMIT 50)
`;
