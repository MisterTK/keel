/**
 * Discovery store: the always-on, cheap per-target traffic ledger written to
 * `.keel/discovery.db` (DX spec §2 — the evidence that makes `keel init`
 * evidence-based). SQLite with ZERO native deps via Node's builtin `node:sqlite`
 * (`DatabaseSync`; floor node>=22.5, declared in engines).
 *
 * The `discovery` table matches the CANONICAL schema owned by
 * `crates/keel-journal` (`src/discovery.rs`) exactly, so one `.keel/discovery.db`
 * is inspectable the same way whether it was written by the Rust core, the
 * Python front end, or this one: one WITHOUT ROWID row per target, WAL mode, and
 * a single self-contained UPSERT that accumulates counters and keeps the
 * latency/seen extremes (`max`/`min`) and the most-recent error.
 *
 * Accounting mirrors the canonical store: a cache hit is a `call` and a
 * `cache_hit` only — it consumed no upstream attempt, so it is neither a
 * `success` nor a `failure`; thus `calls == successes + failures + cache_hits`.
 * `breaker_opens` counts calls that saw an OPEN breaker (fail-fast, KEEL-E012),
 * not breaker transitions. `last_error_*` tracks the most recent failure and is
 * never erased by a later success (coalesce in the UPSERT).
 *
 * Observations are folded into per-target aggregates in memory on the hot path
 * (cheap: integer arithmetic + one injected-clock read) and written once at
 * flush, so discovery never throws into, slows, or adds output to the user's
 * program (DX invariant 4). `node:sqlite` is loaded lazily and synchronously
 * (createRequire) at flush only, so it never loads when Keel is disabled or when
 * nothing was observed. The clock is injectable for deterministic tests.
 */

import { createRequire } from "node:module";
import { mkdirSync } from "node:fs";
import { dirname, join } from "node:path";

export function createDiscovery(cwd = process.cwd(), { now = Date.now } = {}) {
  const dbPath = join(cwd, ".keel", "discovery.db");
  const aggregates = new Map(); // target -> Aggregate

  return {
    dbPath,

    /**
     * Hot-path: fold one intercepted call's Outcome envelope into its target's
     * aggregate. `latencyMs` is the call's end-to-end time (0 if unknown).
     */
    observe(target, outcome, latencyMs = 0) {
      if (!target || outcome == null) return;
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

    /** Persist accumulated aggregates via the canonical UPSERT. Never throws. */
    flushSync() {
      if (aggregates.size === 0) return false;
      try {
        const require = createRequire(import.meta.url);
        const { DatabaseSync } = require("node:sqlite");
        mkdirSync(dirname(dbPath), { recursive: true });
        const db = new DatabaseSync(dbPath);
        try {
          db.exec(CONNECTION_PRAGMAS);
          db.exec(DISCOVERY_SCHEMA);
          const upsert = db.prepare(UPSERT);
          for (const [target, a] of aggregates) {
            const fallback = now();
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
              a.last_seen_ms ?? fallback,
              a.last_error_class,
              a.last_error_status
            );
          }
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
  };
}

const CONNECTION_PRAGMAS = `
PRAGMA journal_mode = WAL;
PRAGMA busy_timeout = 5000;
PRAGMA synchronous = NORMAL;
`;

// Canonical schema — MUST match crates/keel-journal/src/discovery.rs verbatim.
const DISCOVERY_SCHEMA = `
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
   last_error_class, last_error_status)
VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
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
    ELSE last_error_status END
`;
