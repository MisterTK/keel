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
 * idempotent. A function that throws is class `other`, which is NOT in the
 * default retry.on — so by default failures propagate unchanged (no retries);
 * add `other` to the target's retry.on to retry function failures.
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

export function wrapExport(target, fn) {
  const wrapped = async function (...args) {
    const backend = getBackend();
    if (!backend) return fn.apply(this, args); // disabled: transparent passthrough
    const request = { v: 1, target, op: target, idempotent: true, args_hash: hashArgs(args) };
    const self = this;
    const started = performance.now();
    const outcome = await backend.execute(request, async () => {
      try {
        return { status: "ok", payload: await fn.apply(self, args) };
      } catch (err) {
        return { status: "error", class: "other", message: err?.message ?? String(err), original: err };
      }
    });
    getDiscovery()?.observe(target, outcome, performance.now() - started);
    if (outcome.result === "ok") return outcome.payload;
    const orig = outcome.error?.original;
    if (orig instanceof Error) throw attachOutcome(orig, outcome);
    if (orig !== undefined) throw orig;
    const e = new Error(outcome.error?.message ?? "keel failure");
    e.code = outcome.error?.code;
    throw attachOutcome(e, outcome);
  };
  Object.defineProperty(wrapped, "name", { value: fn.name || "keelWrapped", configurable: true });
  return wrapped;
}
