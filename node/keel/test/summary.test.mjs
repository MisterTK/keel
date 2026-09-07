// src/summary.mjs — the exit-time console summary (design spec Part A).
// Wording is pinned by conformance/console_summary/*.json, shared with the
// Python front end: identical counts must print identical bytes.

import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync, readdirSync, mkdtempSync, writeFileSync, chmodSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, delimiter } from "node:path";
import { createSummary, formatSummary, keelOnPath } from "../src/summary.mjs";

const corpusDir = new URL("../../../conformance/console_summary/", import.meta.url);

function outcome(overrides = {}) {
  return {
    v: 1, result: "ok", attempts: 1, from_cache: false, waits_ms: [],
    throttled: false, throttle_wait_ms: 0, breaker: "closed", trace_id: "t-000001",
    ...overrides,
  };
}

test("plain success counts one call", () => {
  const s = createSummary();
  s.observe(outcome(), true);
  assert.deepEqual(s.counts(), {
    calls: 1, throttled: 0, retries_succeeded: 0, breaker_trips: 0,
    cache_hits: 0, not_retried: 0, unprotected: 0,
  });
});

test("retries_succeeded needs ok, not from cache, attempts > 1", () => {
  const s = createSummary();
  s.observe(outcome({ attempts: 3 }), true);
  s.observe(outcome({ result: "error", attempts: 3, error: { code: "KEEL-E010" } }), true);
  s.observe(outcome({ attempts: 2, from_cache: true }), true);
  assert.equal(s.counts().retries_succeeded, 1);
  assert.equal(s.counts().cache_hits, 1);
});

test("error codes classify breaker trips and not-retried; unwrapped counts unprotected", () => {
  const s = createSummary();
  s.observe(outcome({ result: "error", attempts: 0, error: { code: "KEEL-E012" } }), true);
  s.observe(outcome({ result: "error", attempts: 1, error: { code: "KEEL-E014" } }), true);
  s.observe(outcome({ throttled: true }), false);
  const c = s.counts();
  assert.equal(c.breaker_trips, 1);
  assert.equal(c.not_retried, 1);
  assert.equal(c.throttled, 1);
  assert.equal(c.unprotected, 1);
});

test("every corpus case formats byte-identically", () => {
  const files = readdirSync(corpusDir).filter((f) => f.endsWith(".json")).sort();
  assert.ok(files.length >= 5, "corpus present");
  for (const file of files) {
    const c = JSON.parse(readFileSync(new URL(file, corpusDir), "utf8"));
    assert.equal(formatSummary(c.counts, c.keel_on_path), c.expected, c.name);
  }
});

test("keelOnPath scans PATH for a keel executable", () => {
  const dir = mkdtempSync(join(tmpdir(), "keel-path-"));
  assert.equal(keelOnPath({ PATH: dir }), false);
  const exe = join(dir, process.platform === "win32" ? "keel.exe" : "keel");
  writeFileSync(exe, "#!/bin/sh\n");
  chmodSync(exe, 0o755);
  assert.equal(keelOnPath({ PATH: `${tmpdir()}${delimiter}${dir}` }), true);
});
