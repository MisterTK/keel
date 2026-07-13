// `keel sim` fault injection (docs/sim-format.md): the SimBackend effect
// wrapper, its cursor persistence, and directive resolution. All against a
// fake `Backend` — no native addon, no real network — so this leg always
// runs. The real end-to-end path (a real retry loop reacting to an injected
// fault, plus a genuine SIGKILL crash-restart) is exercised by
// `crates/keel-cli/src/sim.rs`'s Rust integration tests.

import test from "node:test";
import assert from "node:assert/strict";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { SIM_CRASH_EXIT_CODE, SimBackend, installSim } from "../src/sim.mjs";

function fakeBackend() {
  return {
    configured: null,
    configure(policy) {
      this.configured = policy;
    },
    async execute(request, effect) {
      return effect(1);
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

/** A minimal duck-typed cursor (the only interface `SimBackend` needs):
 * in-memory, never persisted — real persistence across a restart is exercised
 * separately via two `installSim` calls over the SAME plan path below. */
function memoryCursor() {
  const counts = new Map();
  return {
    nextIndex(target) {
      return counts.get(target) ?? 0;
    },
    bump(target) {
      counts.set(target, this.nextIndex(target) + 1);
    },
  };
}

function tmpDir() {
  return mkdtempSync(join(tmpdir(), "keel-sim-test-"));
}

test("SIM_CRASH_EXIT_CODE is 128 + SIGKILL(9)", () => {
  assert.equal(SIM_CRASH_EXIT_CODE, 137);
});

test("an untargeted call passes through untouched", async () => {
  const backend = new SimBackend(fakeBackend(), { other: [{ kind: "crash" }] }, memoryCursor());
  const outcome = await backend.execute({ target: "t" }, async (a) => ({ status: "ok", payload: a }));
  assert.deepEqual(outcome, { status: "ok", payload: 1 });
});

test("conn/timeout/5xx/429 directives synthesize the adapter shape, never calling the real effect", async () => {
  const faults = { t: [{ kind: "conn" }, { kind: "timeout" }, { kind: "5xx" }, { kind: "429" }] };
  const backend = new SimBackend(fakeBackend(), faults, memoryCursor());
  const never = () => {
    throw new Error("the real effect must never run for an injected fault");
  };
  assert.equal((await backend.execute({ target: "t" }, never)).class, "conn");
  assert.equal((await backend.execute({ target: "t" }, never)).class, "timeout");
  const http5xx = await backend.execute({ target: "t" }, never);
  assert.equal(http5xx.class, "http");
  assert.equal(http5xx.http_status, 503);
  const http429 = await backend.execute({ target: "t" }, never);
  assert.equal(http429.http_status, 429);
});

test("explicit status and retry_after_ms are honored", async () => {
  const backend = new SimBackend(
    fakeBackend(),
    { t: [{ kind: "http", status: 502, retry_after_ms: 250 }] },
    memoryCursor()
  );
  const outcome = await backend.execute({ target: "t" }, async () => ({ status: "ok" }));
  assert.equal(outcome.http_status, 502);
  assert.equal(outcome.retry_after_ms, 250);
});

test("an ok directive still calls the real effect", async () => {
  const backend = new SimBackend(fakeBackend(), { t: [{ kind: "ok" }] }, memoryCursor());
  const calls = [];
  const outcome = await backend.execute({ target: "t" }, async (a) => {
    calls.push(a);
    return { status: "ok" };
  });
  assert.deepEqual(calls, [1]);
  assert.deepEqual(outcome, { status: "ok" });
});

test("repeat serves the same directive across consecutive attempts before advancing", async () => {
  const backend = new SimBackend(fakeBackend(), { t: [{ kind: "timeout", repeat: 2 }, { kind: "ok" }] }, memoryCursor());
  const first = await backend.execute({ target: "t" }, async () => ({ status: "ok", payload: "live" }));
  const second = await backend.execute({ target: "t" }, async () => ({ status: "ok", payload: "live" }));
  const third = await backend.execute({ target: "t" }, async () => ({ status: "ok", payload: "live" }));
  assert.equal(first.class, "timeout");
  assert.equal(second.class, "timeout");
  assert.deepEqual(third, { status: "ok", payload: "live" });
});

test("a spent sequence passes through live", async () => {
  const backend = new SimBackend(fakeBackend(), { t: [{ kind: "ok" }] }, memoryCursor());
  await backend.execute({ target: "t" }, async () => ({ status: "ok" })); // consumes the only directive
  const outcome = await backend.execute({ target: "t" }, async () => ({ status: "ok", payload: "live" }));
  assert.deepEqual(outcome, { status: "ok", payload: "live" });
});

test("a crash directive invokes the injected crash and never the real effect", async () => {
  let crashed = false;
  const backend = new SimBackend(fakeBackend(), { t: [{ kind: "crash" }] }, memoryCursor(), () => {
    crashed = true;
  });
  await assert.rejects(
    backend.execute({ target: "t" }, () => {
      throw new Error("must not run");
    })
  );
  assert.equal(crashed, true);
});

test("configure and report delegate; persistent/layer pass through", () => {
  const inner = fakeBackend();
  const backend = new SimBackend(inner, {}, memoryCursor());
  backend.configure({ x: 1 });
  assert.deepEqual(inner.configured, { x: 1 });
  assert.deepEqual(backend.report(), { reported: true });
  assert.equal(backend.persistent, true);
});

test("installSim loads the faults block and wraps the backend", () => {
  const dir = tmpDir();
  try {
    const planPath = join(dir, "plan.json");
    writeFileSync(planPath, JSON.stringify({ v: 1, target: "x.mjs", faults: { t: [{ kind: "crash" }] } }));
    const backend = installSim(fakeBackend(), { planPath, env: { KEEL_QUIET: "1" } });
    assert.ok(backend instanceof SimBackend);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("a missing faults block is an empty map (no injection at all)", async () => {
  const dir = tmpDir();
  try {
    const planPath = join(dir, "plan.json");
    writeFileSync(planPath, JSON.stringify({ v: 1, target: "x.mjs" }));
    const backend = installSim(fakeBackend(), { planPath, env: { KEEL_QUIET: "1" } });
    const outcome = await backend.execute({ target: "t" }, async () => ({ status: "ok", payload: "live" }));
    assert.deepEqual(outcome, { status: "ok", payload: "live" });
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("the real cursor sidecar persists across separate installSim calls over the same plan (crash-restart continuity)", async () => {
  const dir = tmpDir();
  try {
    const planPath = join(dir, "plan.json");
    writeFileSync(
      planPath,
      JSON.stringify({ v: 1, target: "x.mjs", faults: { t: [{ kind: "timeout" }, { kind: "ok" }] } })
    );
    // "Run 1": consumes directive 0 (timeout) for target t.
    const run1 = installSim(fakeBackend(), { planPath, env: { KEEL_QUIET: "1" } });
    const outcome1 = await run1.execute({ target: "t" }, async () => ({ status: "ok", payload: "live-1" }));
    assert.equal(outcome1.class, "timeout");
    // "Run 2": a FRESH SimBackend/Cursor built the same way a restarted
    // process would (installSim re-reads the same sidecar) — it must pick up
    // at directive 1 (ok), not replay directive 0 again.
    const run2 = installSim(fakeBackend(), { planPath, env: { KEEL_QUIET: "1" } });
    const outcome2 = await run2.execute({ target: "t" }, async () => ({ status: "ok", payload: "live-2" }));
    assert.deepEqual(outcome2, { status: "ok", payload: "live-2" });
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
