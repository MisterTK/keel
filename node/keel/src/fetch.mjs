/**
 * Global `fetch` interception.
 *
 * Design note (why not undici's setGlobalDispatcher): the zero-runtime-deps
 * rule forbids depending on the `undici` package, and Node exposes no public
 * `node:undici` builtin. The stable, dependency-free seam that covers modern
 * Node HTTP is the global `fetch` binding itself — every `undici`/`fetch` call
 * in user code (and libraries that use global fetch) flows through it. We wrap
 * `globalThis.fetch`; the effect forwards to the ORIGINAL fetch, so uninstalling
 * restores byte-identical behavior.
 *
 * Behavior contract (DX invariants):
 *   - success path is never changed: a real HTTP response (any 2xx/3xx, or a
 *     non-429 4xx) is returned unchanged — the actual Response object, body
 *     intact (we only ever read status + Retry-After header, never the body).
 *   - only 429 / ≥500 are treated as transient and retried per policy.
 *   - after exhausting retries on a transient HTTP status, the LAST real
 *     Response is returned unchanged (not thrown) — the app still sees its 503.
 *   - a thrown transport error (conn/timeout) is re-thrown UNCHANGED after the
 *     final attempt, with a non-enumerable `keelOutcome` attached.
 *   - non-idempotent calls (POST without an idempotency key) are observed, not
 *     retried (KEEL-E014).
 */

import {
  normalizeRequest,
  resolveTarget,
  isIdempotent,
  argsHash,
  parseRetryAfter,
  classifyThrow,
  isTransientStatus,
} from "./judge.mjs";
import { attachOutcome } from "./runtime.mjs";
import { KeelError } from "./engine.mjs";

const isResponse = (v) => typeof Response !== "undefined" && v instanceof Response;

/** Cancel a response body we are discarding, quietly. */
function cancelBody(resp) {
  if (resp && resp.body && !resp.bodyUsed) {
    try {
      resp.body.cancel().catch(() => {});
    } catch {
      /* already locked/consumed */
    }
  }
}

/**
 * Install the wrapping fetch. Returns an uninstall function that restores the
 * exact original. Idempotent: a second install is a no-op.
 */
export function installFetch(backend, discovery, { globalObj = globalThis } = {}) {
  const original = globalObj.fetch;
  if (typeof original !== "function") return () => {};
  if (original.__keelWrapped) return () => {}; // already installed

  const keelFetch = async function (input, init) {
    const { url, method, headers, body } = normalizeRequest(input, init);
    let parsed;
    try {
      parsed = new URL(url);
    } catch {
      // Not a wrappable URL — do nothing, forward verbatim (Level 0 rule).
      return original.call(this, input, init);
    }
    const hostname = parsed.hostname;
    const target = resolveTarget(hostname);
    const op = `${method} ${hostname}${parsed.pathname}`;
    const idemHeader = readIdempotencyHeader(backend, target);
    const idempotent = isIdempotent(method, headers, idemHeader);
    // args_hash (cache/journal key material) is derived ONLY for idempotent GET
    // requests, per the brief — cross-language parity with the Python twin.
    const hash = method === "GET" ? argsHash(method, parsed.href, body) : null;
    const request = { v: 1, target, op, idempotent, args_hash: hash };

    // Per-attempt timeout is enforced only for idempotent calls: a timeout we
    // impose becomes a new thrown error, and we must never inject one into a
    // non-idempotent success path.
    const timeoutMs = idempotent ? durationMs(backend.layer(target, "timeout")) : null;

    // Track the last transient (5xx/429) response. On a retry it is superseded
    // and its body cancelled; only the response we actually return keeps its
    // body — so retries never leave an undrained body (no undici warnings, no
    // memory leak), and the success path is untouched.
    let held = null;

    const outcome = await backend.execute(request, async () => {
      const { signal, cancel } = withTimeout(init?.signal, timeoutMs);
      const attemptInit = signal ? { ...init, signal } : init;
      try {
        const resp = await original.call(this, input, attemptInit);
        if (isTransientStatus(resp.status)) {
          if (held && held !== resp) cancelBody(held);
          held = resp;
          return {
            status: "error",
            class: "http",
            http_status: resp.status,
            retry_after_ms: parseRetryAfter(resp.headers.get("retry-after")),
            message: `HTTP ${resp.status}`,
            original: resp,
          };
        }
        cancelBody(held); // a good response supersedes any held transient
        held = null;
        return { status: "ok", payload: resp };
      } catch (err) {
        return {
          status: "error",
          class: classifyThrow(err),
          message: err?.message ?? String(err),
          original: err,
        };
      } finally {
        cancel();
      }
    });

    discovery?.observe(target, hostname);

    if (outcome.result === "ok") return attachOutcome(outcome.payload, outcome);

    const orig = outcome.error?.original;
    if (isResponse(orig)) return attachOutcome(orig, outcome); // last real HTTP response, unchanged
    cancelBody(held); // dangling transient before a thrown transport error
    if (orig instanceof Error) throw attachOutcome(orig, outcome); // original transport error, unchanged
    // No captured original (e.g. breaker open before any attempt): surface a KeelError.
    const e = new KeelError(outcome.error?.code ?? "KEEL-E040", outcome.error?.message ?? "keel failure");
    throw attachOutcome(e, outcome);
  };

  Object.defineProperty(keelFetch, "name", { value: "fetch", configurable: true });
  keelFetch.__keelWrapped = true;
  keelFetch.__keelOriginal = original;
  globalObj.fetch = keelFetch;

  return function uninstall() {
    if (globalObj.fetch === keelFetch) globalObj.fetch = original;
  };
}

function readIdempotencyHeader(backend, target) {
  const idem = backend.layer(target, "idempotency");
  return idem && typeof idem === "object" ? idem.header : undefined;
}

function durationMs(v) {
  const m = /^(\d+)(ms|s|m|h)$/.exec(String(v ?? "").trim());
  if (!m) return null;
  const mult = { ms: 1, s: 1000, m: 60000, h: 3600000 }[m[2]];
  return Number(m[1]) * mult;
}

/** Compose the caller's AbortSignal with a Keel timeout; returns a cleanup. */
function withTimeout(callerSignal, timeoutMs) {
  if (!timeoutMs || timeoutMs <= 0) return { signal: callerSignal, cancel() {} };
  const controller = new AbortController();
  const onAbort = () => controller.abort(callerSignal?.reason);
  if (callerSignal) {
    if (callerSignal.aborted) controller.abort(callerSignal.reason);
    else callerSignal.addEventListener("abort", onAbort, { once: true });
  }
  const timer = setTimeout(() => controller.abort(new DOMException("Keel timeout", "TimeoutError")), timeoutMs);
  if (typeof timer.unref === "function") timer.unref();
  return {
    signal: controller.signal,
    cancel() {
      clearTimeout(timer);
      callerSignal?.removeEventListener?.("abort", onAbort);
    },
  };
}
