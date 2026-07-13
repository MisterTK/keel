// Tier 2 flow designation and the durable-flow run path (dx-spec §1 Level 2).
// Mirrors python/keel/tests/test_flows.py's three layers:
//   * pure parsing/matching (extractFlowEntrypoints, matchFlow),
//   * runAsFlow orchestration against a fake backend (no native addon needed),
//   * the native binding replay round-trip + async ordering rule (native only).

import test from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { DatabaseSync } from "node:sqlite";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { extractFlowEntrypoints } from "../src/policy.mjs";
import {
  matchFlow,
  runAsFlow,
  backendSupportsFlows,
  backendHasJournal,
} from "../src/flow.mjs";
import { loaded as nativeLoaded, KeelCore } from "../../keel-core-native/index.mjs";

const gate = nativeLoaded
  ? {}
  : { skip: "keel-core-native binary absent — build with `cargo build -p keel-node --release`" };

/**
 * Hand-rolled bare MessagePack for a small string->string map — the exact
 * bytes `crates/keel-core/src/flow.rs`'s `decode_payload` falls back to
 * decoding when a payload carries no `keel.step/v1` schema tag (the "legacy
 * bare messagepack still decodes" path its own unit test pins). Fixstr-only
 * (every key/value here is well under 32 bytes), so a single-byte header per
 * string suffices. Parity with the Python twin's `_pack_bare_str_map` in
 * python/keel/tests/test_flows.py.
 */
function packBareStrMap(pairs) {
  const parts = [Buffer.from([0x80 | pairs.length])];
  for (const [k, v] of pairs) {
    for (const s of [k, v]) {
      const raw = Buffer.from(s, "utf8");
      if (raw.length >= 32) throw new Error("fixstr only; not needed for test-sized values");
      parts.push(Buffer.from([0xa0 | raw.length]), raw);
    }
  }
  return Buffer.concat(parts);
}

/**
 * Directly journal a `running` (unterminated) step record carrying an
 * adapter-injected idempotency key — models a crash mid-effect: the exact
 * shape `FlowHandle::run_live` writes BEFORE firing the effect (a real crash
 * between that write and the terminal record leaves precisely this row).
 * Same technique conformance/scenarios' JSON-driven interpreter uses via its
 * `inject_running` field (crates/keel-core/tests/flows_conformance.rs),
 * applied directly against the on-disk journal.db so a test proves the REAL
 * napi binding surface (`recordedIdempotencyKey`, `executeAsync`'s
 * `idempotencyKey` parameter) rather than re-deriving the core-level proof
 * `crates/keel-core/tests/idempotency.rs` already carries.
 */
function injectRunningStep(journalPath, flowId, { seq, stepKey, idempotencyKey }) {
  const db = new DatabaseSync(journalPath);
  try {
    const payload = packBareStrMap([["idempotency_key", idempotencyKey]]);
    db.prepare(
      "INSERT INTO steps (flow_id, seq, step_key, kind, attempt, outcome, payload, error_class, started_at, ended_at) " +
        "VALUES (?,?,?,?,?,?,?,?,?,?)"
    ).run(flowId, seq, stepKey, "effect", 0, "running", payload, null, Date.now(), null);
  } finally {
    db.close();
  }
}

// --- extractFlowEntrypoints ---------------------------------------------------

test("extractFlowEntrypoints parses ts:<glob>#<export>", () => {
  const got = extractFlowEntrypoints({ flows: { entrypoints: ["ts:pipeline.mjs#main"] } });
  assert.deepEqual(got, [{ raw: "ts:pipeline.mjs#main", glob: "pipeline.mjs", fn: "main" }]);
});

test("extractFlowEntrypoints skips malformed and non-ts entries", () => {
  const got = extractFlowEntrypoints({
    flows: { entrypoints: ["ts:nofn", "py:m:f", "ts:m#f", 7, null] },
  });
  assert.deepEqual(got.map((e) => e.raw), ["ts:m#f"]);
});

test("extractFlowEntrypoints skips a glob in the export-name position", () => {
  // The flow body must always be a concrete, named export.
  const got = extractFlowEntrypoints({ flows: { entrypoints: ["ts:jobs/*.mjs#*"] } });
  assert.deepEqual(got, []);
});

test("extractFlowEntrypoints: absent [flows] is empty", () => {
  assert.deepEqual(extractFlowEntrypoints({}), []);
  assert.deepEqual(extractFlowEntrypoints({ flows: {} }), []);
  assert.deepEqual(extractFlowEntrypoints({ flows: { entrypoints: "not-an-array" } }), []);
});

// --- matchFlow -----------------------------------------------------------------

test("matchFlow matches a concrete relative path", () => {
  const entries = [{ raw: "ts:pipeline.mjs#main", glob: "pipeline.mjs", fn: "main" }];
  const got = matchFlow("pipeline.mjs", "/app", entries);
  assert.equal(got, entries[0]);
  assert.equal(matchFlow("other.mjs", "/app", entries), null);
});

test("matchFlow: no entries never matches", () => {
  assert.equal(matchFlow("pipeline.mjs", "/app", []), null);
});

test("matchFlow: a concrete entry with a directory component matches only that path", () => {
  const entries = [{ raw: "ts:jobs/pipeline.mjs#main", glob: "jobs/pipeline.mjs", fn: "main" }];
  assert.equal(matchFlow("jobs/pipeline.mjs", "/app", entries), entries[0]);
  assert.equal(matchFlow("other/pipeline.mjs", "/app", entries), null);
});

test("matchFlow: a glob entry resolves to the concrete matched path", () => {
  const entries = [{ raw: "ts:jobs/*.mjs#run", glob: "jobs/*.mjs", fn: "run" }];
  const got = matchFlow("jobs/ingest.mjs", "/app", entries);
  assert.ok(got);
  assert.equal(got.raw, "ts:jobs/ingest.mjs#run");
  assert.equal(got.fn, "run");
  assert.equal(got.via, "ts:jobs/*.mjs#run");
});

test("matchFlow: two scripts under one glob get independent identities", () => {
  const entries = [{ raw: "ts:jobs/*.mjs#run", glob: "jobs/*.mjs", fn: "run" }];
  const a = matchFlow("jobs/ingest.mjs", "/app", entries);
  const b = matchFlow("jobs/export.mjs", "/app", entries);
  assert.notEqual(a.raw, b.raw);
  assert.equal(a.raw, "ts:jobs/ingest.mjs#run");
  assert.equal(b.raw, "ts:jobs/export.mjs#run");
});

test("matchFlow: a concrete entry wins over a matching glob", () => {
  const entries = [
    { raw: "ts:jobs/*.mjs#run", glob: "jobs/*.mjs", fn: "run" },
    { raw: "ts:jobs/ingest.mjs#special", glob: "jobs/ingest.mjs", fn: "special" },
  ];
  const got = matchFlow("jobs/ingest.mjs", "/app", entries);
  assert.equal(got, entries[1]);
  assert.equal(got.via, undefined);
});

test("matchFlow: glob also matches by basename", () => {
  const entries = [{ raw: "ts:*.mjs#main", glob: "*.mjs", fn: "main" }];
  const got = matchFlow("nested/dir/pipeline.mjs", "/app", entries);
  assert.ok(got);
  assert.equal(got.fn, "main");
});

// --- runAsFlow against a fake backend (no native addon needed) ---------------

class FakeFlowBackend {
  entered = [];
  exited = [];
  executed = 0;
  times = [];
  persistent;
  #replay;
  constructor({ replay = false, persistent = true } = {}) {
    this.#replay = replay;
    this.persistent = persistent;
  }
  enterFlow(entrypoint, argsHash, opts = {}) {
    this.entered.push([entrypoint, argsHash, opts]);
    return {
      flow_id: "fid-1",
      status: this.#replay ? "completed" : "running",
      replay: this.#replay,
    };
  }
  exitFlow(status) {
    this.exited.push(status);
  }
  journalTime(key, nowMs) {
    this.times.push(nowMs);
    return this.#replay ? 424242 : nowMs;
  }
  journalRandom(key, data) {
    return data;
  }
}

function writeModule(dir, name, body) {
  writeFileSync(join(dir, `${name}.mjs`), body);
  return { raw: `ts:${name}.mjs#main`, glob: `${name}.mjs`, fn: "main" };
}

test("runAsFlow: enters, runs, and completes", async (t) => {
  const dir = mkdtempSync(join(tmpdir(), "keel-flow-run-"));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const entry = writeModule(
    dir,
    "flowmod_ok",
    `export async function main() {
      // A virtualized read inside the flow.
      globalThis.__SEEN_TIME = Date.now();
    }\n`
  );
  const backend = new FakeFlowBackend();
  const origDateNow = Date.now;
  let exitCode;
  const origExit = process.exit;
  process.exit = (code) => {
    exitCode = code;
    throw new Error("__process_exit__");
  };
  try {
    await assert.rejects(
      runAsFlow(join(dir, "flowmod_ok.mjs"), entry, backend, [], { env: { KEEL_QUIET: "1" } }),
      /__process_exit__/
    );
  } finally {
    process.exit = origExit;
  }
  assert.equal(exitCode, 0);
  assert.equal(backend.entered.length, 1);
  assert.equal(backend.exited.pop(), "completed");
  assert.equal(backend.times.length, 1, "Date.now was virtualized in-flow");
  assert.equal(Date.now, origDateNow, "Date.now restored after the flow");
});

test("runAsFlow: a thrown error marks the flow failed and exits 1", async (t) => {
  const dir = mkdtempSync(join(tmpdir(), "keel-flow-run-fail-"));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const entry = writeModule(dir, "flowmod_boom", `export function main() { throw new Error("boom"); }\n`);
  const backend = new FakeFlowBackend();
  const origExit = process.exit;
  const origError = console.error;
  let exitCode;
  process.exit = (code) => {
    exitCode = code;
    throw new Error("__process_exit__");
  };
  console.error = () => {};
  try {
    await assert.rejects(
      runAsFlow(join(dir, "flowmod_boom.mjs"), entry, backend, [], { env: { KEEL_QUIET: "1" } }),
      /__process_exit__/
    );
  } finally {
    process.exit = origExit;
    console.error = origError;
  }
  assert.equal(exitCode, 1);
  assert.deepEqual(backend.exited, ["failed"]);
});

test("runAsFlow: a replayed (already-completed) flow is never demoted on error", async (t) => {
  // A rerun of an already-COMPLETED flow that throws (e.g. a replay-miss after
  // a code change) must NOT be stamped 'failed' — that would re-open a
  // finished flow for live re-execution.
  const dir = mkdtempSync(join(tmpdir(), "keel-flow-run-replay-err-"));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const entry = writeModule(
    dir,
    "flowmod_replay_err",
    `export function main() { throw new Error("changed code / replay miss"); }\n`
  );
  const backend = new FakeFlowBackend({ replay: true });
  const origExit = process.exit;
  const origError = console.error;
  process.exit = (code) => {
    throw new Error("__process_exit__:" + code);
  };
  console.error = () => {};
  try {
    await assert.rejects(
      runAsFlow(join(dir, "flowmod_replay_err.mjs"), entry, backend, [], { env: { KEEL_QUIET: "1" } }),
      /__process_exit__:1/
    );
  } finally {
    process.exit = origExit;
    console.error = origError;
  }
  assert.deepEqual(backend.exited, [], "completed flow must not be demoted to failed");
});

test("runAsFlow: stub backend (no enterFlow/exitFlow) is a precise unsupported error", async () => {
  const entry = { raw: "ts:pipeline.mjs#main", glob: "pipeline.mjs", fn: "main" };
  const stubLike = { execute: () => ({}) }; // no enterFlow/exitFlow
  assert.equal(backendSupportsFlows(stubLike), false);
  const origExit = process.exit;
  let exitCode;
  process.exit = (code) => {
    exitCode = code;
    throw new Error("__process_exit__");
  };
  try {
    await assert.rejects(
      runAsFlow("/tmp/pipeline.mjs", entry, stubLike, [], { env: { KEEL_QUIET: "1" } }),
      /__process_exit__/
    );
  } finally {
    process.exit = origExit;
  }
  assert.equal(exitCode, 1);
});

test("runAsFlow: a native-shaped backend with no journal is refused before enterFlow", async () => {
  const entry = { raw: "ts:pipeline.mjs#main", glob: "pipeline.mjs", fn: "main" };
  const backend = new FakeFlowBackend({ persistent: false });
  assert.equal(backendSupportsFlows(backend), true);
  assert.equal(backendHasJournal(backend), false);
  const origExit = process.exit;
  let exitCode;
  process.exit = (code) => {
    exitCode = code;
    throw new Error("__process_exit__");
  };
  try {
    await assert.rejects(
      runAsFlow("/tmp/pipeline.mjs", entry, backend, [], { env: { KEEL_QUIET: "1" } }),
      /__process_exit__/
    );
  } finally {
    process.exit = origExit;
  }
  assert.equal(exitCode, 1);
  assert.deepEqual(backend.entered, [], "enterFlow must NOT be reached without a journal");
});

// --- native binding replay round-trip + async ordering rule ------------------

function tmpJournalCore(t) {
  const dir = mkdtempSync(join(tmpdir(), "keel-flow-native-"));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const core = new KeelCore({ journalPath: join(dir, "journal.db") });
  core.configure({});
  return core;
}

test("native: a completed flow replays without refiring effects", gate, async (t) => {
  const core = tmpJournalCore(t);
  let fires = 0;
  const eff = async () => {
    fires += 1;
    return { status: "ok", payload: { i: fires } };
  };

  core.enterFlow("ts:pipeline.mjs#main", "ah-1", "ch-1");
  for (let i = 0; i < 3; i++) {
    const out = await core.executeAsync(
      { v: 1, target: "api.x", op: "api.x", args_hash: `h${i}`, idempotent: true },
      eff
    );
    assert.equal(out.result, "ok");
  }
  const t1 = core.journalTime("ts:Date.now#-", 1783728000);
  core.exitFlow("completed");
  assert.equal(fires, 3);
  assert.equal(t1, 1783728000);

  // Resume: completed → pure replay. No effect re-fires; recorded values
  // (payloads, time) are substituted.
  const info = core.enterFlow("ts:pipeline.mjs#main", "ah-1", "ch-1");
  assert.equal(info.status, "completed");
  assert.equal(info.replay, true);
  for (let i = 0; i < 3; i++) {
    const out = await core.executeAsync(
      { v: 1, target: "api.x", op: "api.x", args_hash: `h${i}`, idempotent: true },
      eff
    );
    assert.deepEqual(out.payload, { i: i + 1 });
  }
  const t2 = core.journalTime("ts:Date.now#-", 9999);
  core.exitFlow("completed");
  assert.equal(fires, 3, "replay fired no effects");
  assert.equal(t2, 1783728000, "time replayed");
});

test("native: idempotency key recorded on crash survives resume", gate, async (t) => {
  // contracts/adapter-pack.md "Idempotency-key injection" rule 3, through the
  // REAL napi binding (the gap this test closes: executeAsync did not expose
  // idempotencyKey, and there was no binding-level recordedIdempotencyKey peek
  // at all). Step 1 completes normally with key "ik-1"; step 2's crash is
  // modeled by directly journaling its `running` record (injectRunningStep)
  // carrying key "ik-2". On resume: step 1's peek misses (it is terminal) and
  // its effect never re-fires; step 2's peek must resurface "ik-2" — and the
  // re-executed live effect, injected with THAT peeked key, must actually run
  // with the SAME key as the crashed attempt, not merely with *a* key.
  const dir = mkdtempSync(join(tmpdir(), "keel-flow-idem-native-"));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  const journalPath = join(dir, "journal.db");
  const entrypoint = "ts:pipeline.mjs#main";
  const argsHash = "ah-idem-native";
  const flowId = `${entrypoint}#${argsHash}#`;

  const core1 = new KeelCore({ journalPath });
  core1.configure({});
  core1.enterFlow(entrypoint, argsHash, "ch-1");
  const step1Key = "api.pay.example#c1";
  const step2Key = "api.pay.example#c2";
  assert.equal(core1.recordedIdempotencyKey(step1Key), null);
  const out1 = await core1.executeAsync(
    { v: 1, target: "api.pay.example", op: "POST x", args_hash: "c1", idempotent: true },
    async () => ({ status: "ok", payload: { charge: "ch_1" } }),
    "ik-1"
  );
  assert.equal(out1.result, "ok");
  assert.equal(core1.recordedIdempotencyKey(step2Key), null);

  // Crash: step 2's `running` record (carrying ik-2) is journaled directly,
  // never through a live executeAsync call — modeling the process dying
  // between the running-write and the terminal outcome.
  injectRunningStep(journalPath, flowId, { seq: 2, stepKey: step2Key, idempotencyKey: "ik-2" });

  const core2 = new KeelCore({ journalPath });
  core2.configure({});
  const info = core2.enterFlow(entrypoint, argsHash, "ch-1");
  assert.equal(info.replay, false, "an uncompleted flow resumes live, not a pure replay");

  // Step 1 is terminal: the peek misses, and a resumed re-execution (with a
  // DIFFERENT, would-be-wrong key) is substituted, not re-fired.
  assert.equal(core2.recordedIdempotencyKey(step1Key), null);
  let fires = 0;
  const out1b = await core2.executeAsync(
    { v: 1, target: "api.pay.example", op: "POST x", args_hash: "c1", idempotent: true },
    async () => {
      fires += 1;
      return { status: "ok", payload: { charge: "never" } };
    },
    "ik-should-be-ignored"
  );
  assert.deepEqual(out1b.payload, { charge: "ch_1" }, "a terminal step is substituted");
  assert.equal(fires, 0, "a substituted step must not fire its effect");

  // Step 2: the crashed `running` record resurfaces its key — the
  // load-bearing assertion is exact equality with the crashed attempt's key,
  // not merely that SOME key came back.
  const peeked = core2.recordedIdempotencyKey(step2Key);
  assert.equal(peeked, "ik-2", "the peek must resurface the SAME key the crashed attempt recorded");

  const out2 = await core2.executeAsync(
    { v: 1, target: "api.pay.example", op: "POST x", args_hash: "c2", idempotent: true },
    async () => ({ status: "ok", payload: { charge: "ch_2" } }),
    peeked
  );
  assert.equal(out2.result, "ok");
  assert.equal(out2.attempts, 1);
  core2.exitFlow("completed");
});

test("native: a flow requires a journal (KEEL-E040)", gate, () => {
  const core = new KeelCore(); // in-memory, no journal
  core.configure({});
  assert.throws(
    () => core.enterFlow("ts:pipeline.mjs#main", "ah-1"),
    (err) => err.code === "KEEL-E040"
  );
});

test("native: synchronous execute() is refused while a flow is open (KEEL-E005)", gate, async (t) => {
  const core = tmpJournalCore(t);
  core.enterFlow("ts:pipeline.mjs#main", "ah-sync-refuse", "ch-1");
  try {
    assert.throws(
      () => core.execute({ v: 1, target: "api.x", op: "api.x", idempotent: true }, () => ({ status: "ok" })),
      (err) => err.code === "KEEL-E005"
    );
  } finally {
    core.exitFlow("completed");
  }
});

test("native: concurrent effects inside one flow are serialized in await/claim order", gate, async (t) => {
  const core = tmpJournalCore(t);
  core.enterFlow("ts:pipeline.mjs#main", "ah-concurrent");
  const order = [];
  function step(n, delayMs) {
    return core.executeAsync(
      { v: 1, target: "api.x", op: "api.x", args_hash: `h${n}`, idempotent: true },
      async () => {
        order.push(`start-${n}`);
        await new Promise((r) => setTimeout(r, delayMs));
        order.push(`end-${n}`);
        return { status: "ok", payload: n };
      }
    );
  }
  // Step 1 has the LONGER internal delay; if calls were not serialized by
  // claim (handle-entry) order, step 2's shorter delay would let it interleave
  // (start-1, start-2, end-2, end-1). The ordering rule requires strict
  // admission: one step at a time, claimed in the order the calls reach the
  // handle, regardless of which finishes first.
  const [o1, o2] = await Promise.all([step(1, 40), step(2, 1)]);
  assert.deepEqual(order, ["start-1", "end-1", "start-2", "end-2"]);
  assert.equal(o1.payload, 1);
  assert.equal(o2.payload, 2);
  core.exitFlow("completed");
});

test("native: a value read racing a concurrently in-flight step passes through, not deadlocks", gate, async (t) => {
  const core = tmpJournalCore(t);
  core.enterFlow("ts:pipeline.mjs#main", "ah-race");
  const pending = core.executeAsync(
    { v: 1, target: "api.x", op: "api.x", idempotent: true },
    async () => {
      await new Promise((r) => setTimeout(r, 150));
      return { status: "ok", payload: 1 };
    }
  );
  const started = Date.now();
  const value = core.journalTime("ts:Date.now#-", 42);
  const elapsed = Date.now() - started;
  assert.equal(value, 42, "unjournaled passthrough while a step is in flight");
  assert.ok(elapsed < 100, `journalTime must not block on the in-flight step (took ${elapsed}ms)`);
  const outcome = await pending;
  assert.equal(outcome.result, "ok");
  core.exitFlow("completed");
});

test("native: exitFlow with an unawaited in-flight effect fails loud (KEEL-E040), not a hang", gate, async (t) => {
  const core = tmpJournalCore(t);
  core.enterFlow("ts:pipeline.mjs#main", "ah-exit-race");
  // Fire an effect WITHOUT awaiting it (a misuse the ordering rule warns
  // against), then immediately try to exit the flow while it is still in
  // flight — exitFlow must throw a precise error rather than block the JS
  // thread forever (the in-flight step can only resolve its own Promise by
  // running JS on this SAME thread).
  const pending = core.executeAsync(
    { v: 1, target: "api.x", op: "api.x", idempotent: true },
    async () => {
      await new Promise((r) => setTimeout(r, 100));
      return { status: "ok", payload: 1 };
    }
  );
  assert.throws(
    () => core.exitFlow("completed"),
    (err) => err.code === "KEEL-E040"
  );
  const outcome = await pending; // let it drain so the handle is free to close
  assert.equal(outcome.result, "ok");
  core.exitFlow("completed"); // now uncontended — succeeds
});

test("native: enterFlow with a prior unawaited in-flight effect fails loud (KEEL-E040), not a hang", gate, async (t) => {
  const core = tmpJournalCore(t);
  core.enterFlow("ts:pipeline.mjs#first", "ah-enter-race");
  // Fire an effect WITHOUT awaiting it, then immediately try to enter a
  // SECOND flow on the same core while the first flow's step is still in
  // flight — enterFlow must throw a precise error rather than block the JS
  // thread forever (the in-flight step can only resolve its own Promise by
  // running JS on this SAME thread, so a blocking wait here could never
  // return).
  const pending = core.executeAsync(
    { v: 1, target: "api.x", op: "api.x", idempotent: true },
    async () => {
      await new Promise((r) => setTimeout(r, 100));
      return { status: "ok", payload: 1 };
    }
  );
  assert.throws(
    () => core.enterFlow("ts:pipeline.mjs#second", "ah-enter-race-2"),
    (err) => err.code === "KEEL-E040"
  );
  const outcome = await pending; // let it drain so the first flow's slot frees up
  assert.equal(outcome.result, "ok");
  core.exitFlow("completed"); // close the first flow — enterFlow never got to stash
  // "second"'s handle (the refusal happened before that), so it was never
  // left open here; a real re-entry of "second" would still hit KEEL-E030
  // (lease held) until its 30s default lease expires, since the refused
  // attempt's journal/lease work runs to completion before the slot check —
  // that lease-recovery path is exercised by the flow-lease-held-and-takeover
  // conformance scenario, not repeated here.
});
