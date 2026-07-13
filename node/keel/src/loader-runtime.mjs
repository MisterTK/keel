/**
 * Main-thread runtime for `ts:` function targets. The ESM loader rewrites a
 * matching module's named function export `foo` into:
 *
 *     function __keel$foo(...) { <original body> }
 *     export const foo = __keel$wrap("ts:glob#foo", __keel$foo);
 *
 * and injects `import { wrapExport as __keel$wrap } from "<abs loader-runtime>"`.
 * The rewritten module runs on the MAIN thread, so `wrapExport` here shares the
 * bootstrap's backend via runtime.mjs.
 *
 * Contract: listing a `ts:` target in keel.toml is the user's assertion that
 * the function is safe to retry, so wrapped function targets are treated as
 * idempotent by default. A function that throws is class `other`, which is
 * NOT in the default retry.on — so by default failures propagate unchanged
 * (no retries); add `other` to the target's retry.on to retry function
 * failures.
 *
 * `wrapExport`'s `idempotent` option (default `true`) lets a caller override
 * that default — used by the eve pack (packs/eve.mjs), whose `tool:<name>`
 * targets are discovered automatically rather than opted into by name in
 * keel.toml, so they default to non-idempotent (Level 0 hard rule: never
 * retry a call the developer didn't explicitly bless).
 */

import { createHash } from "node:crypto";
import { getBackend, getDiscovery, attachOutcome } from "./runtime.mjs";

function hashArgs(args) {
  try {
    return createHash("sha256").update(JSON.stringify(args)).digest("hex");
  } catch {
    return null; // non-serializable args → disables caching for this call
  }
}

/** A JSON-safe view of a value for the core payload (the native core requires
 *  a serde-serializable `payload`; the stub tolerates any object). We keep the
 *  live value side-band and hand it back on the live path, so this only matters
 *  for a cache STORE — a non-serializable result simply becomes uncacheable. */
function jsonSafe(v) {
  try {
    JSON.stringify(v);
    return v;
  } catch {
    return null;
  }
}

export function wrapExport(target, fn, { idempotent = true } = {}) {
  const wrapped = async function (...args) {
    const backend = getBackend();
    if (!backend) return fn.apply(this, args); // disabled: transparent passthrough
    const request = { v: 1, target, op: target, idempotent, args_hash: hashArgs(args) };
    const self = this;
    const started = performance.now();
    // Keep the live result / error side-band so the core payload can stay JSON
    // (byte-transparent live delivery; the native core cannot round-trip an
    // opaque object). Only a cache HIT falls back to the round-tripped payload.
    let liveResult;
    let haveResult = false;
    let liveErr;
    const outcome = await backend.execute(request, async () => {
      try {
        liveResult = await fn.apply(self, args);
        haveResult = true;
        return { status: "ok", payload: jsonSafe(liveResult) };
      } catch (err) {
        liveErr = err;
        return { status: "error", class: "other", message: err?.message ?? String(err) };
      }
    });
    getDiscovery()?.observe(target, outcome, performance.now() - started);
    if (outcome.result === "ok") {
      // Live call → the real return value, unchanged; cache hit → the replayed
      // (JSON) payload (in-process, or across runs under the persistent journal).
      return haveResult && !outcome.from_cache ? liveResult : outcome.payload;
    }
    if (liveErr instanceof Error) throw attachOutcome(liveErr, outcome);
    if (liveErr !== undefined) throw liveErr;
    const e = new Error(outcome.error?.message ?? "keel failure");
    e.code = outcome.error?.code;
    throw attachOutcome(e, outcome);
  };
  Object.defineProperty(wrapped, "name", { value: fn.name || "keelWrapped", configurable: true });
  return wrapped;
}

/**
 * Runtime half of the eve pack's `defineTool` rewrite (packs/eve.mjs's
 * `transformEveTool` injects a call to this for every rewritten module). Wraps
 * only the tool definition's `execute` function through `wrapExport` — with
 * `idempotent: false` (eve's filesystem-discovered tools get no per-target
 * opt-in, unlike a `ts:` target) — and hands eve back an otherwise-identical
 * definition object, so `description`/`inputSchema`/anything else the real
 * `defineTool` needs passes through untouched.
 *
 * `def` shapes that don't match the documented eve convention (no function
 * `execute`) are passed to the real `defineTool` completely untouched — a
 * pack never changes success-path semantics for a shape it doesn't recognize.
 */
export function wrapEveTool(target, realDefineTool, def) {
  if (def === null || typeof def !== "object" || typeof def.execute !== "function") {
    return realDefineTool(def);
  }
  return realDefineTool({ ...def, execute: wrapExport(target, def.execute, { idempotent: false }) });
}
