/**
 * Discovery store: an always-on, cheap aggregate of observed traffic written to
 * `.keel/discovery.db` (DX spec §2 — the evidence that makes `keel init`
 * evidence-based). SQLite with ZERO native deps via Node's builtin
 * `node:sqlite` (DatabaseSync; floor node>=22.5, declared in engines).
 *
 * Counters are authoritative from the backend's own per-target report (single
 * source of truth); this store only accumulates them into the db and unions the
 * set of hosts seen per target. Everything is best-effort: discovery must never
 * throw into, slow, or add output to the user's program (DX invariant 4).
 *
 * `node:sqlite` is loaded lazily and synchronously (createRequire) at flush
 * time, so it is never loaded when Keel is disabled and never at import.
 */

import { createRequire } from "node:module";
import { mkdirSync } from "node:fs";
import { dirname, join } from "node:path";

const SCHEMA_VERSION = 1;

export function createDiscovery(cwd = process.cwd()) {
  const dbPath = join(cwd, ".keel", "discovery.db");
  const hosts = new Map(); // target -> Set<host>

  return {
    dbPath,
    /** Hot-path, trivial: note a host seen for a target (evidence for init). */
    observe(target, host) {
      if (!host) return;
      let set = hosts.get(target);
      if (!set) hosts.set(target, (set = new Set()));
      set.add(host);
    },
    /** Persist a backend report synchronously. Never throws. */
    flushSync(report) {
      const targets = report?.targets;
      if (!targets || Object.keys(targets).length === 0) return false;
      try {
        const require = createRequire(import.meta.url);
        const { DatabaseSync } = require("node:sqlite");
        mkdirSync(dirname(dbPath), { recursive: true });
        const db = new DatabaseSync(dbPath);
        try {
          db.exec(SCHEMA);
          db.prepare("INSERT INTO meta(key,value) VALUES('schema_version',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value").run(String(SCHEMA_VERSION));
          db.prepare("INSERT INTO meta(key,value) VALUES('envelope_version','1') ON CONFLICT(key) DO NOTHING").run();
          const now = Date.now();
          const upsert = db.prepare(UPSERT);
          const readHosts = db.prepare("SELECT hosts FROM discovery WHERE target=?");
          for (const [target, c] of Object.entries(targets)) {
            const prior = readHosts.get(target);
            const merged = mergeHosts(prior?.hosts, hosts.get(target));
            upsert.run(
              target,
              c.calls ?? 0,
              c.attempts ?? 0,
              c.retries ?? 0,
              c.successes ?? 0,
              c.failures ?? 0,
              c.cache_hits ?? 0,
              c.throttled ?? 0,
              c.breaker_opens ?? 0,
              c.breaker_state ?? "closed",
              merged,
              now
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

function mergeHosts(priorJson, newSet) {
  const set = new Set();
  if (priorJson) {
    try {
      for (const h of JSON.parse(priorJson)) set.add(h);
    } catch {
      /* ignore corrupt prior value */
    }
  }
  if (newSet) for (const h of newSet) set.add(h);
  return set.size ? JSON.stringify([...set].sort()) : null;
}

const SCHEMA = `
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT);
CREATE TABLE IF NOT EXISTS discovery (
  target        TEXT PRIMARY KEY,
  calls         INTEGER NOT NULL DEFAULT 0,
  attempts      INTEGER NOT NULL DEFAULT 0,
  retries       INTEGER NOT NULL DEFAULT 0,
  successes     INTEGER NOT NULL DEFAULT 0,
  failures      INTEGER NOT NULL DEFAULT 0,
  cache_hits    INTEGER NOT NULL DEFAULT 0,
  throttled     INTEGER NOT NULL DEFAULT 0,
  breaker_opens INTEGER NOT NULL DEFAULT 0,
  breaker_state TEXT,
  hosts         TEXT,
  last_seen_ms  INTEGER
);
`;

const UPSERT = `
INSERT INTO discovery
  (target, calls, attempts, retries, successes, failures, cache_hits, throttled, breaker_opens, breaker_state, hosts, last_seen_ms)
VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
ON CONFLICT(target) DO UPDATE SET
  calls         = calls + excluded.calls,
  attempts      = attempts + excluded.attempts,
  retries       = retries + excluded.retries,
  successes     = successes + excluded.successes,
  failures      = failures + excluded.failures,
  cache_hits    = cache_hits + excluded.cache_hits,
  throttled     = throttled + excluded.throttled,
  breaker_opens = breaker_opens + excluded.breaker_opens,
  breaker_state = excluded.breaker_state,
  hosts         = excluded.hosts,
  last_seen_ms  = excluded.last_seen_ms
`;
