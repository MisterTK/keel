// Offline replay (docs/recording-format.md): Recording parsing and
// ReplayBackend's request-matching rule (target+args_hash, else
// target+op, FIFO within a group, loud failure on a miss).

import test from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { Recording, ReplayBackend, UnmatchedEffectError, installReplay } from "../src/testing.mjs";
import { AsyncEngine } from "../src/engine.mjs";
import { setRuntime, getBackend } from "../src/runtime.mjs";

function writeRecording(lines) {
  const dir = mkdtempSync(join(tmpdir(), "keel-testing-"));
  const path = join(dir, "r.ndjson");
  writeFileSync(path, lines.map((l) => JSON.stringify(l)).join("\n") + "\n");
  return { dir, path };
}

const META = { v: 1, type: "meta", id: "r1", language: "node", target: "app.mjs", args: [], started_at_ms: 0, redacted_headers: [] };

function call(seq, target, op, argsHash, outcome) {
  return { v: 1, type: "call", seq, target, op, idempotent: argsHash !== null, args_hash: argsHash, attempts: 1, latency_ms: 1, body_captured: true, outcome };
}

test("Recording.load parses the meta header and call lines, skipping non-call lines", () => {
  const { dir, path } = writeRecording([
    META,
    call(1, "api.example.com", "GET api.example.com/x", "h1", { result: "ok", payload: 1 }),
    { v: 1, type: "future-kind" },
  ]);
  const rec = Recording.load(path);
  assert.equal(rec.meta.id, "r1");
  assert.equal(rec.calls.length, 1);
  assert.equal(rec.calls[0].target, "api.example.com");
  rmSync(dir, { recursive: true, force: true });
});

test("Recording.load rejects a file with no meta header", () => {
  const { dir, path } = writeRecording([{ v: 1, type: "call", seq: 1 }]);
  assert.throws(() => Recording.load(path), /no meta header/);
  rmSync(dir, { recursive: true, force: true });
});

test("Recording.load rejects an empty file", () => {
  const dir = mkdtempSync(join(tmpdir(), "keel-testing-"));
  const path = join(dir, "empty.ndjson");
  writeFileSync(path, "");
  assert.throws(() => Recording.load(path), /is empty/);
  rmSync(dir, { recursive: true, force: true });
});

test("ReplayBackend matches by target+args_hash when args_hash is present", async () => {
  const outcome = { result: "ok", payload: { x: 1 } };
  const rec = new Recording(META, [call(1, "api.example.com", "GET api.example.com/x", "h1", outcome)]);
  const backend = new ReplayBackend(rec);
  const got = await backend.execute({ target: "api.example.com", op: "GET api.example.com/x", args_hash: "h1" });
  // `from_cache` is forced true on a served "ok" (see the dedicated test
  // below) — assert the rest of the shape here.
  assert.deepEqual(got, { ...outcome, from_cache: true });
});

test("ReplayBackend falls back to target+op when args_hash is null on both sides", async () => {
  const outcome = { result: "error", error: { code: "KEEL-E010" } };
  const rec = new Recording(META, [call(1, "api.example.com", "POST api.example.com/y", null, outcome)]);
  const backend = new ReplayBackend(rec);
  const got = await backend.execute({ target: "api.example.com", op: "POST api.example.com/y", args_hash: null });
  assert.deepEqual(got, outcome);
});

test("ReplayBackend serves repeated identical calls in recorded (FIFO) order", async () => {
  const first = { result: "ok", payload: 1 };
  const second = { result: "ok", payload: 2 };
  const rec = new Recording(META, [
    call(1, "api.example.com", "GET api.example.com/x", "h1", first),
    call(2, "api.example.com", "GET api.example.com/x", "h1", second),
  ]);
  const backend = new ReplayBackend(rec);
  const req = { target: "api.example.com", op: "GET api.example.com/x", args_hash: "h1" };
  assert.deepEqual(await backend.execute(req), { ...first, from_cache: true });
  assert.deepEqual(await backend.execute(req), { ...second, from_cache: true });
});

test("ReplayBackend throws UnmatchedEffectError on a novel call, never falling through live", async () => {
  const rec = new Recording(META, [call(1, "api.example.com", "GET api.example.com/x", "h1", { result: "ok" })]);
  const backend = new ReplayBackend(rec);
  await assert.rejects(
    () => backend.execute({ target: "api.example.com", op: "GET api.example.com/x", args_hash: "different" }),
    UnmatchedEffectError
  );
});

test("ReplayBackend throws once a group's recorded calls are exhausted", async () => {
  const rec = new Recording(META, [call(1, "api.example.com", "GET api.example.com/x", "h1", { result: "ok" })]);
  const backend = new ReplayBackend(rec);
  const req = { target: "api.example.com", op: "GET api.example.com/x", args_hash: "h1" };
  await backend.execute(req);
  await assert.rejects(() => backend.execute(req), UnmatchedEffectError);
});

test("ReplayBackend forces from_cache=true on a served ok outcome (no live object exists to hand back)", async () => {
  const rec = new Recording(META, [
    call(1, "api.example.com", "GET api.example.com/x", "h1", { result: "ok", payload: { x: 1 }, from_cache: false }),
  ]);
  const backend = new ReplayBackend(rec);
  const got = await backend.execute({ target: "api.example.com", op: "GET api.example.com/x", args_hash: "h1" });
  assert.equal(got.from_cache, true);
  assert.deepEqual(got.payload, { x: 1 });
});

test("ReplayBackend leaves an error outcome's shape untouched", async () => {
  const errorOutcome = { result: "error", error: { code: "KEEL-E010" } };
  const rec = new Recording(META, [call(1, "api.example.com", "POST api.example.com/y", null, errorOutcome)]);
  const backend = new ReplayBackend(rec);
  const got = await backend.execute({ target: "api.example.com", op: "POST api.example.com/y", args_hash: null });
  assert.deepEqual(got, errorOutcome);
});

test("ReplayBackend never invokes the caller's real effect", async () => {
  const rec = new Recording(META, [call(1, "api.example.com", "GET api.example.com/x", "h1", { result: "ok" })]);
  const backend = new ReplayBackend(rec);
  let ran = false;
  await backend.execute({ target: "api.example.com", op: "GET api.example.com/x", args_hash: "h1" }, async () => {
    ran = true;
  });
  assert.equal(ran, false);
});

test("ReplayBackend exposes layer()/resolveTarget()/persistent so fetch.mjs's unconditional calls don't throw", () => {
  const backend = new ReplayBackend(new Recording(META, []));
  assert.equal(backend.layer("x", "y"), undefined);
  assert.equal(backend.persistent, false);
  assert.deepEqual(backend.report(), {});
  // No recorded policy to pattern-match against: the LLM host map/Vertex
  // suffix rule still applies (policy-independent), everything else falls
  // back to the bare host — matching pre-Task-11 behavior (installReplay
  // never installed `[target]` pattern matchers either). Python's
  // `ReplayBackend` matches this exactly too as of issue #53 (it used to
  // return the bare host unconditionally, with no LLM mapping) — parity with
  // `test_testing.py`'s
  // `test_resolve_target_falls_back_to_a_bare_unconfigured_stub_with_no_resolver`.
  assert.equal(backend.resolveTarget("POST", "api.openai.com"), "llm:openai");
  assert.equal(backend.resolveTarget("GET", "api.example.com"), "api.example.com");
});

// Reproduces and pins the issue #51 fix: `installReplay` must thread the
// backend active immediately before the swap into `ReplayBackend` as a
// `resolver`, so a recording made under a policy with `[target]` pattern
// keys replays against the SAME target the original resolver would have
// computed (docs/recording-format.md rule 1) — not an unconfigured engine's
// pattern-blind reduction. Parity with the Python twin's
// `InstallReplayResolveTargetTest`.
test("installReplay delegates resolveTarget to the previously-active backend, patterns included", async () => {
  const real = new AsyncEngine();
  real.configure({ target: { "api.*.example.com": { retry: { attempts: 2 } } } });
  // Sanity: this is NOT the bare-host/LLM-map fallback — it exercises the
  // real `[target]` pattern-matching logic, so a passing assertion below
  // actually proves delegation reaches it.
  const expected = real.resolveTarget("GET", "api.foo.example.com");
  assert.equal(expected, "api.*.example.com");

  setRuntime({ enabled: true, backend: real, discovery: null });
  const { dir, path } = writeRecording([META]);
  const uninstall = installReplay(path);
  try {
    const backend = getBackend();
    assert.ok(backend instanceof ReplayBackend);
    assert.equal(backend.resolveTarget("GET", "api.foo.example.com"), expected);
  } finally {
    uninstall();
    setRuntime({ enabled: false, backend: null, discovery: null });
    rmSync(dir, { recursive: true, force: true });
  }
});

test("installReplay delegates layer() to the previously-active backend too", async () => {
  const real = new AsyncEngine();
  real.configure({ target: { "api.example.com": { retry: { attempts: 3 } } } });
  const expected = real.layer("api.example.com", "retry");

  setRuntime({ enabled: true, backend: real, discovery: null });
  const { dir, path } = writeRecording([META]);
  const uninstall = installReplay(path);
  try {
    const backend = getBackend();
    assert.deepEqual(backend.layer("api.example.com", "retry"), expected);
  } finally {
    uninstall();
    setRuntime({ enabled: false, backend: null, discovery: null });
    rmSync(dir, { recursive: true, force: true });
  }
});
