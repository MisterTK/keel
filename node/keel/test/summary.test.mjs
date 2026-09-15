// src/summary.mjs — the exit-time console summary (design spec Part A).
// Wording is pinned by conformance/console_summary/*.json, shared with the
// Python front end: identical counts must print identical bytes.

import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync, readdirSync, mkdtempSync, writeFileSync, chmodSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, delimiter } from "node:path";
import { createSummary, formatSummary, formatSummaryJson, keelOnPath } from "../src/summary.mjs";
import { jsonLogs } from "../src/log.mjs";

const corpusDir = new URL("../../../conformance/console_summary/", import.meta.url);
const jsonCorpusDir = new URL("../../../conformance/console_summary_json/", import.meta.url);

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

// `KEEL_LOG_FORMAT=json` (F10): a second shared corpus, read by the Python
// front end too — identical counts + meta must produce identical BYTES
// (sorted keys, no spaces), or a log pipeline sees two different schemas.
test("every json corpus case formats byte-identically", () => {
  const files = readdirSync(jsonCorpusDir).filter((f) => f.endsWith(".json")).sort();
  assert.ok(files.length >= 3, "json corpus present");
  for (const file of files) {
    const c = JSON.parse(readFileSync(new URL(file, jsonCorpusDir), "utf8"));
    assert.equal(formatSummaryJson(c.counts, c.meta), c.expected, c.name);
  }
});

test("only the exact value json switches the log format", () => {
  for (const v of ["json", "JSON", "  json  ", "Json"]) assert.equal(jsonLogs({ KEEL_LOG_FORMAT: v }), true, v);
  for (const v of ["", "1", "true", "ndjson", "text", "jsonl"]) assert.equal(jsonLogs({ KEEL_LOG_FORMAT: v }), false, v);
  assert.equal(jsonLogs({}), false);
});

test("keelOnPath scans PATH for a keel executable", () => {
  const dir = mkdtempSync(join(tmpdir(), "keel-path-"));
  assert.equal(keelOnPath({ PATH: dir }), false);
  const exe = join(dir, process.platform === "win32" ? "keel.exe" : "keel");
  writeFileSync(exe, "#!/bin/sh\n");
  chmodSync(exe, 0o755);
  assert.equal(keelOnPath({ PATH: `${tmpdir()}${delimiter}${dir}` }), true);
});

import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const hookUrl = new URL("../hook.mjs", import.meta.url).href;
const fetchOnce = fileURLToPath(new URL("../fixtures/fetch-once.mjs", import.meta.url));
// Either bridge form is correct (depends on whether `keel` is installed here).

function runFixture(policy, env = {}) {
  const cwd = mkdtempSync(join(tmpdir(), "keel-summary-"));
  if (policy !== null) writeFileSync(join(cwd, "keel.toml"), policy);
  const cleanEnv = { ...process.env };
  for (const k of ["KEEL_DISABLE", "KEEL_QUIET", "KEEL_BACKEND", "KEEL_CWD"]) delete cleanEnv[k];
  return spawnSync(process.execPath, ["--import", hookUrl, fetchOnce], {
    cwd,
    env: { ...cleanEnv, ...env },
    encoding: "utf8",
  });
}

test("summary prints on stderr after one wrapped call; stdout untouched", () => {
  const r = runFixture('[target."127.0.0.1"]\n');
  assert.equal(r.status, 0, r.stderr);
  assert.equal(r.stdout, "status 200\n");
  assert.match(r.stderr, /keel ▸ wrapped global fetch/);
  assert.match(r.stderr, /keel ▸ 1 call\n {7}(uvx --from keelrun-cli )?keel report --open for the full picture\n/);
  assert.ok(r.stderr.indexOf("wrapped") < r.stderr.indexOf("report --open"), "banner before summary");
});

test("a call on a target with no policy entry is reported unprotected", () => {
  const r = runFixture(null);
  assert.equal(r.status, 0, r.stderr);
  assert.match(r.stderr, /keel ▸ 1 call · 1 call unprotected\n/);
});

test("telemetry.console = false silences the summary but not the banner", () => {
  const r = runFixture('[target."127.0.0.1"]\n[telemetry]\nconsole = false\n');
  assert.equal(r.status, 0, r.stderr);
  assert.match(r.stderr, /keel ▸ wrapped/);
  assert.doesNotMatch(r.stderr, /report --open/);
});

test("KEEL_QUIET silences the summary", () => {
  const r = runFixture('[target."127.0.0.1"]\n', { KEEL_QUIET: "1" });
  assert.equal(r.status, 0, r.stderr);
  assert.doesNotMatch(r.stderr, /keel ▸/);
});

test("KEEL_DISABLE prints nothing at all", () => {
  const r = runFixture('[target."127.0.0.1"]\n', { KEEL_DISABLE: "1" });
  assert.equal(r.status, 0, r.stderr);
  assert.doesNotMatch(r.stderr, /keel ▸/);
});
