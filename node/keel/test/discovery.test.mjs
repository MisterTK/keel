// Discovery folds each intercepted call's Outcome into a per-target aggregate
// and writes .keel/discovery.db via node:sqlite. The table is pinned to the
// CANONICAL schema owned by crates/keel-journal (src/discovery.rs) so it cannot
// drift; this test asserts the schema (PRAGMA table_info), the accounting, the
// latency/error columns, and the accumulating UPSERT (min/max/coalesce).

import test from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createRequire } from "node:module";
import { createDiscovery, SCHEMA_VERSION, RETENTION_DAYS, MS_PER_DAY } from "../src/discovery.mjs";

const require = createRequire(import.meta.url);
const { DatabaseSync } = require("node:sqlite");

const ok = (attempts) => ({ v: 1, result: "ok", attempts, from_cache: false, error: null });
const cacheHit = () => ({ v: 1, result: "ok", attempts: 0, from_cache: true, error: null });
const fail = (attempts, cls, status, code = "KEEL-E010") => ({
  v: 1,
  result: "error",
  attempts,
  from_cache: false,
  error: { code, class: cls, http_status: status },
});

function withDir(fn) {
  const dir = mkdtempSync(join(tmpdir(), "keel-disc-"));
  try {
    return fn(dir);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

// The canonical column list — order, type, NOT NULL, and PK all pinned.
const CANONICAL_COLUMNS = [
  { name: "target", type: "TEXT", notnull: 1, pk: 1 }, // WITHOUT ROWID PK ⇒ implicitly NOT NULL
  { name: "calls", type: "INTEGER", notnull: 1, pk: 0 },
  { name: "attempts", type: "INTEGER", notnull: 1, pk: 0 },
  { name: "retries", type: "INTEGER", notnull: 1, pk: 0 },
  { name: "successes", type: "INTEGER", notnull: 1, pk: 0 },
  { name: "failures", type: "INTEGER", notnull: 1, pk: 0 },
  { name: "cache_hits", type: "INTEGER", notnull: 1, pk: 0 },
  { name: "throttled", type: "INTEGER", notnull: 1, pk: 0 },
  { name: "breaker_opens", type: "INTEGER", notnull: 1, pk: 0 },
  { name: "total_latency_ms", type: "INTEGER", notnull: 1, pk: 0 },
  { name: "max_latency_ms", type: "INTEGER", notnull: 1, pk: 0 },
  { name: "first_seen_ms", type: "INTEGER", notnull: 1, pk: 0 },
  { name: "last_seen_ms", type: "INTEGER", notnull: 1, pk: 0 },
  { name: "last_error_class", type: "TEXT", notnull: 0, pk: 0 },
  { name: "last_error_status", type: "INTEGER", notnull: 0, pk: 0 },
  { name: "not_retried", type: "INTEGER", notnull: 1, pk: 0 },
  { name: "unwrapped_calls", type: "INTEGER", notnull: 1, pk: 0 },
];

test("discovery.db uses the canonical schema (WITHOUT ROWID, no meta/hosts)", () => {
  withDir((dir) => {
    const disc = createDiscovery(dir, { now: () => 1000 });
    disc.observe("api.x", ok(1), 5);
    assert.equal(disc.flushSync(), true);

    const db = new DatabaseSync(join(dir, ".keel", "discovery.db"));
    try {
      const cols = db
        .prepare("PRAGMA table_info(discovery)")
        .all()
        .map((c) => ({ name: c.name, type: c.type, notnull: c.notnull, pk: c.pk }));
      assert.deepEqual(cols, CANONICAL_COLUMNS);

      const ddl = db.prepare("SELECT sql FROM sqlite_master WHERE name='discovery'").get();
      assert.match(ddl.sql, /WITHOUT\s+ROWID/i, "table is WITHOUT ROWID");

      // no divergent tables (meta) and no divergent columns (hosts/breaker_state).
      const meta = db.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name='meta'").get();
      assert.equal(meta, undefined, "no meta table");
      assert.ok(!cols.some((c) => c.name === "hosts" || c.name === "breaker_state"));
    } finally {
      db.close();
    }
  });
});

test("aggregates one target with canonical accounting, latency, and last error", () => {
  withDir((dir) => {
    let clock = 1000;
    const disc = createDiscovery(dir, { now: () => clock });
    disc.observe("api.x", ok(1), 100);
    clock = 1010;
    disc.observe("api.x", ok(2), 300); // one retry
    clock = 1020;
    disc.observe("api.x", cacheHit(), 2); // call + cache_hit only
    clock = 1030;
    disc.observe("api.x", fail(3, "http", 503), 50); // terminal failure
    assert.equal(disc.flushSync(), true);

    const db = new DatabaseSync(join(dir, ".keel", "discovery.db"));
    try {
      const r = db.prepare("SELECT * FROM discovery WHERE target=?").get("api.x");
      assert.equal(r.calls, 4);
      assert.equal(r.successes, 2);
      assert.equal(r.cache_hits, 1);
      assert.equal(r.failures, 1);
      assert.equal(r.calls, r.successes + r.failures + r.cache_hits, "calls = successes+failures+cache_hits");
      assert.equal(r.attempts, 6); // 1 + 2 + 0 + 3
      assert.equal(r.retries, 3); // (1-1) + (2-1) + 0 + (3-1) = 0+1+0+2
      assert.equal(r.total_latency_ms, 452); // 100+300+2+50
      assert.equal(r.max_latency_ms, 300);
      assert.equal(r.first_seen_ms, 1000);
      assert.equal(r.last_seen_ms, 1030);
      assert.equal(r.last_error_class, "http");
      assert.equal(r.last_error_status, 503);
    } finally {
      db.close();
    }
  });
});

test("retries counts attempts beyond the first, summed", () => {
  withDir((dir) => {
    const disc = createDiscovery(dir, { now: () => 1 });
    disc.observe("t", ok(1), 1); // 0 retries
    disc.observe("t", ok(3), 1); // 2 retries
    disc.observe("t", cacheHit(), 1); // 0 retries (attempts 0)
    disc.flushSync();
    const db = new DatabaseSync(join(dir, ".keel", "discovery.db"));
    try {
      const r = db.prepare("SELECT attempts, retries FROM discovery WHERE target='t'").get();
      assert.equal(r.attempts, 4);
      assert.equal(r.retries, 2);
    } finally {
      db.close();
    }
  });
});

test("breaker_opens counts calls that saw an OPEN breaker (KEEL-E012)", () => {
  withDir((dir) => {
    const disc = createDiscovery(dir, { now: () => 1 });
    disc.observe("t", fail(0, "other", null, "KEEL-E012"), 0);
    disc.observe("t", fail(1, "conn", null, "KEEL-E010"), 5); // not a breaker-open
    disc.flushSync();
    const db = new DatabaseSync(join(dir, ".keel", "discovery.db"));
    try {
      const r = db.prepare("SELECT breaker_opens, failures FROM discovery WHERE target='t'").get();
      assert.equal(r.breaker_opens, 1);
      assert.equal(r.failures, 2);
    } finally {
      db.close();
    }
  });
});

test("UPSERT accumulates across flushes: min/max seen, coalesced last error", () => {
  withDir((dir) => {
    // First run: a failure at t=5000.
    const run1 = createDiscovery(dir, { now: () => 5000 });
    run1.observe("api.x", fail(2, "http", 503), 50);
    run1.flushSync();

    // Second run (fresh aggregates → same file): a later success at t=6000 with
    // no error — must NOT erase the last error, and must extend last_seen only.
    const run2 = createDiscovery(dir, { now: () => 6000 });
    run2.observe("api.x", ok(1), 10);
    run2.flushSync();

    const db = new DatabaseSync(join(dir, ".keel", "discovery.db"));
    try {
      const r = db.prepare("SELECT * FROM discovery WHERE target='api.x'").get();
      assert.equal(r.calls, 2);
      assert.equal(r.successes, 1);
      assert.equal(r.failures, 1);
      assert.equal(r.first_seen_ms, 5000, "min across runs");
      assert.equal(r.last_seen_ms, 6000, "max across runs");
      assert.equal(r.max_latency_ms, 50, "max across runs");
      assert.equal(r.total_latency_ms, 60);
      assert.equal(r.last_error_class, "http", "later success does not erase the last error");
      assert.equal(r.last_error_status, 503);
    } finally {
      db.close();
    }
  });
});

test("flush is a no-op (returns false) when nothing was observed", () => {
  withDir((dir) => {
    const disc = createDiscovery(dir);
    assert.equal(disc.flushSync(), false);
  });
});

test("KEEL-E014 counts as not_retried (observed, not retried)", () => {
  withDir((dir) => {
    const disc = createDiscovery(dir, { now: () => 1 });
    disc.observe("t", fail(1, "other", null, "KEEL-E014"), 0);
    disc.observe("t", fail(1, "other", null, "KEEL-E010"), 0); // not E014
    disc.flushSync();
    const db = new DatabaseSync(join(dir, ".keel", "discovery.db"));
    try {
      const r = db.prepare("SELECT not_retried, failures FROM discovery WHERE target='t'").get();
      assert.equal(r.not_retried, 1);
      assert.equal(r.failures, 2);
    } finally {
      db.close();
    }
  });
});

test("a target with an explicit policy entry is wrapped; others are not", () => {
  withDir((dir) => {
    const disc = createDiscovery(dir, { now: () => 1, knownTargets: new Set(["api.example.com"]) });
    disc.observe("api.example.com", ok(1), 1);
    disc.observe("api.other.com", ok(1), 1);
    disc.flushSync();
    const db = new DatabaseSync(join(dir, ".keel", "discovery.db"));
    try {
      const wrapped = db.prepare("SELECT unwrapped_calls FROM discovery WHERE target=?").get("api.example.com");
      const unwrapped = db.prepare("SELECT unwrapped_calls FROM discovery WHERE target=?").get("api.other.com");
      assert.equal(wrapped.unwrapped_calls, 0);
      assert.equal(unwrapped.unwrapped_calls, 1);
    } finally {
      db.close();
    }
  });
});

test("no knownTargets means every call is counted unwrapped", () => {
  withDir((dir) => {
    const disc = createDiscovery(dir, { now: () => 1 });
    disc.observe("api.example.com", ok(1), 1);
    disc.flushSync();
    const db = new DatabaseSync(join(dir, ".keel", "discovery.db"));
    try {
      const r = db.prepare("SELECT unwrapped_calls FROM discovery WHERE target=?").get("api.example.com");
      assert.equal(r.unwrapped_calls, 1);
    } finally {
      db.close();
    }
  });
});

test("a daily bucket is written alongside the lifetime row", () => {
  withDir((dir) => {
    const disc = createDiscovery(dir, { now: () => 5 * MS_PER_DAY + 1234 });
    disc.observe("t", ok(2), 10); // 1 retry
    disc.flushSync();
    const db = new DatabaseSync(join(dir, ".keel", "discovery.db"));
    try {
      const bucket = db.prepare("SELECT * FROM discovery_daily WHERE target='t'").get();
      assert.equal(bucket.day, 5);
      assert.equal(bucket.calls, 1);
      assert.equal(bucket.retries, 1);
    } finally {
      db.close();
    }
  });
});

test("PRAGMA user_version is stamped to the current schema version", () => {
  withDir((dir) => {
    const disc = createDiscovery(dir, { now: () => 1 });
    disc.observe("t", ok(1), 1);
    disc.flushSync();
    const db = new DatabaseSync(join(dir, ".keel", "discovery.db"));
    try {
      const { user_version } = db.prepare("PRAGMA user_version").get();
      assert.equal(user_version, SCHEMA_VERSION);
    } finally {
      db.close();
    }
  });
});

test("a legacy (v1) discovery.db is migrated in place, preserving rows", () => {
  withDir((dir) => {
    const dbDir = join(dir, ".keel");
    require("node:fs").mkdirSync(dbDir, { recursive: true });
    const dbPath = join(dbDir, "discovery.db");
    const legacy = new DatabaseSync(dbPath);
    legacy.exec(`
      CREATE TABLE discovery (
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
    `);
    legacy.prepare(
      "INSERT INTO discovery VALUES ('api.old', 10, 12, 2, 8, 2, 0, 1, 0, 500, 90, ?, ?, 'http', 503)"
    ).run(1_783_728_000_000, 1_783_728_001_000);
    legacy.close();

    const disc = createDiscovery(dir, { now: () => 1_783_728_005_000, knownTargets: new Set(["api.old"]) });
    disc.observe("api.old", ok(1), 5);
    assert.equal(disc.flushSync(), true);

    const db = new DatabaseSync(dbPath);
    try {
      const { user_version } = db.prepare("PRAGMA user_version").get();
      assert.equal(user_version, SCHEMA_VERSION);
      const r = db.prepare("SELECT * FROM discovery WHERE target='api.old'").get();
      assert.equal(r.calls, 11, "old row's count plus the new call");
      assert.equal(r.last_error_status, 503, "legacy data preserved");
      assert.equal(r.not_retried, 0);
      assert.equal(r.unwrapped_calls, 0, "recorded call was wrapped");
      const daily = db.prepare("SELECT COUNT(*) AS n FROM discovery_daily").get();
      assert.equal(daily.n, 1, "the daily table now exists with one bucket");
    } finally {
      db.close();
    }
  });
});

test("daily buckets older than retention are pruned when the day advances", () => {
  withDir((dir) => {
    const dbDir = join(dir, ".keel");
    require("node:fs").mkdirSync(dbDir, { recursive: true });
    const dbPath = join(dbDir, "discovery.db");

    // Seed a fresh v2 file with a stale bucket, then flush a live call on a
    // much later day — the flush path must prune it.
    const seed = createDiscovery(dir, { now: () => 0 });
    seed.observe("t", ok(1), 1); // creates day-0 bucket + stamps schema
    seed.flushSync();

    const later = createDiscovery(dir, { now: () => (RETENTION_DAYS + 10) * MS_PER_DAY });
    later.observe("t", ok(1), 1);
    later.flushSync();

    const db = new DatabaseSync(dbPath);
    try {
      const days = db.prepare("SELECT day FROM discovery_daily ORDER BY day").all().map((r) => r.day);
      assert.ok(!days.includes(0), "the stale day-0 bucket was pruned");
      assert.deepEqual(days, [RETENTION_DAYS + 10]);
    } finally {
      db.close();
    }
  });
});
