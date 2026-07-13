// `keel record run` capture (docs/recording-format.md): the RecordingBackend
// tee, redaction, and body-captured detection. All against a fake `Backend`
// — no native addon, no real HTTP — so this leg always runs.

import test from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  RECORDING_VERSION,
  DEFAULT_REDACT_HEADERS,
  RecordingBackend,
  installRecording,
  redactHeadersFromEnv,
} from "../src/record.mjs";

function readLines(path) {
  return readFileSync(path, "utf8")
    .split("\n")
    .filter((l) => l.length > 0)
    .map((l) => JSON.parse(l));
}

function fakeBackend(outcomes) {
  const calls = [];
  return {
    calls,
    configure(policy) {
      this.configuredWith = policy;
    },
    async execute(request, effect) {
      calls.push(request);
      // The real effect is invoked exactly as a real backend would (a real
      // call happened) — recording must never change this.
      await effect(1);
      return outcomes.shift();
    },
    report() {
      return { reported: true };
    },
    get persistent() {
      return true;
    },
    layer() {
      return undefined;
    },
  };
}

test("redactHeadersFromEnv merges KEEL_RECORD_REDACT_HEADERS with the defaults", () => {
  const set = redactHeadersFromEnv({ KEEL_RECORD_REDACT_HEADERS: "x-custom, X-Another" });
  assert.ok(set.has("authorization"), "defaults survive");
  assert.ok(set.has("x-custom"));
  assert.ok(set.has("x-another"));
  assert.equal(set.size, DEFAULT_REDACT_HEADERS.size + 2);
});

test("RecordingBackend forwards execute unchanged and never invokes effect itself", async () => {
  const outcome = { v: 1, result: "ok", payload: { hi: 1 }, attempts: 1, from_cache: false };
  const inner = fakeBackend([outcome]);
  const dir = mkdtempSync(join(tmpdir(), "keel-record-"));
  const path = join(dir, "r.ndjson");
  const writer = { writeCall: () => {}, writeMeta: () => {} };
  const rb = new RecordingBackend(inner, writer, DEFAULT_REDACT_HEADERS);

  let effectRan = false;
  const request = { v: 1, target: "api.example.com", op: "GET api.example.com/x", idempotent: true, args_hash: "h1" };
  const result = await rb.execute(request, async () => {
    effectRan = true;
    return { status: "ok", payload: { hi: 1 } };
  });

  assert.equal(result, outcome, "the real backend's outcome is returned unchanged");
  assert.equal(effectRan, true, "the real effect actually ran exactly once (via the real backend)");
  assert.equal(inner.calls.length, 1);
  assert.deepEqual(inner.calls[0], request, "the request reaches the real backend unmodified");

  rmSync(dir, { recursive: true, force: true });
});

test("RecordingBackend delegates configure/report/persistent/layer", async () => {
  const inner = fakeBackend([]);
  const rb = new RecordingBackend(inner, { writeCall: () => {} }, DEFAULT_REDACT_HEADERS);
  rb.configure({ target: {} });
  assert.deepEqual(inner.configuredWith, { target: {} });
  assert.deepEqual(rb.report(), { reported: true });
  assert.equal(rb.persistent, true);
  assert.equal(rb.layer("x", "y"), undefined);
});

test("installRecording writes a meta header then one call line per execute, redacting auth headers", async () => {
  const outcome = {
    v: 1,
    result: "ok",
    payload: {
      __keel_http__: 1,
      status: 200,
      headers: [
        ["content-type", "application/json"],
        ["Authorization", "Bearer secret-token"],
      ],
      body_b64: "eyJvayI6dHJ1ZX0=",
    },
    attempts: 1,
    from_cache: false,
  };
  const inner = fakeBackend([outcome]);
  const dir = mkdtempSync(join(tmpdir(), "keel-record-"));
  const path = join(dir, "0000000000001-0000.ndjson");

  const rb = installRecording(inner, {
    path,
    target: "app.mjs",
    args: ["--flag"],
    env: { KEEL_QUIET: "1" },
  });

  const request = {
    v: 1,
    target: "api.example.com",
    op: "GET api.example.com/x",
    idempotent: true,
    args_hash: "abc123",
  };
  await rb.execute(request, async () => ({ status: "ok", payload: {} }));

  const lines = readLines(path);
  assert.equal(lines.length, 2);
  assert.equal(lines[0].type, "meta");
  assert.equal(lines[0].v, RECORDING_VERSION);
  assert.equal(lines[0].id, "0000000000001-0000");
  assert.equal(lines[0].language, "node");
  assert.equal(lines[0].target, "app.mjs");
  assert.deepEqual(lines[0].args, ["--flag"]);
  assert.deepEqual(lines[0].redacted_headers, [...DEFAULT_REDACT_HEADERS].sort());

  assert.equal(lines[1].type, "call");
  assert.equal(lines[1].seq, 1);
  assert.equal(lines[1].target, "api.example.com");
  assert.equal(lines[1].op, "GET api.example.com/x");
  assert.equal(lines[1].idempotent, true);
  assert.equal(lines[1].args_hash, "abc123");
  assert.equal(lines[1].body_captured, true);
  const headers = lines[1].outcome.payload.headers;
  const auth = headers.find(([k]) => k === "Authorization");
  assert.equal(auth[1], "[REDACTED]");
  const ct = headers.find(([k]) => k === "content-type");
  assert.equal(ct[1], "application/json", "non-secret headers pass through unchanged");

  rmSync(dir, { recursive: true, force: true });
});

test("body_captured is false when the outcome payload has no buffered body", async () => {
  const outcome = {
    v: 1,
    result: "error",
    error: { code: "KEEL-E010", class: "http", message: "HTTP 503" },
    attempts: 3,
    from_cache: false,
  };
  const inner = fakeBackend([outcome]);
  const dir = mkdtempSync(join(tmpdir(), "keel-record-"));
  const path = join(dir, "rec.ndjson");
  const rb = installRecording(inner, { path, target: "app.mjs", args: [], env: { KEEL_QUIET: "1" } });
  await rb.execute(
    { v: 1, target: "api.example.com", op: "POST api.example.com/y", idempotent: false, args_hash: null },
    async () => ({ status: "error", class: "http", http_status: 503, message: "HTTP 503" })
  );
  const lines = readLines(path);
  assert.equal(lines[1].body_captured, false);
  assert.equal(lines[1].outcome.result, "error");
  rmSync(dir, { recursive: true, force: true });
});
