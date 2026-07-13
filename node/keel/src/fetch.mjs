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
 *     retried (KEEL-E014). A call whose body is an unbuffered stream is likewise
 *     not retried (it cannot be re-sent); in-memory bodies are re-sent unchanged
 *     on each attempt.
 *   - an LLM POST (llm:* target) still derives an args_hash from its canonical
 *     JSON body (the documented dev-cache exception) so identical prompts replay
 *     from cache — without being made retryable.
 */

import {
  normalizeRequest,
  resolvePolicyTarget,
  isIdempotent,
  resolveIdempotencyInjection,
  defaultMintIdempotencyKey,
  deriveArgsHash,
  parseRetryAfter,
  classifyThrow,
  isTransientStatus,
  responseEnvelope,
  rebuildResponse,
} from "./judge.mjs";
import { attachOutcome } from "./runtime.mjs";
import { KeelError } from "./engine.mjs";
import { durationMs } from "./packs/_shared.mjs";

function isTable(v) {
  return v !== null && typeof v === "object" && !Array.isArray(v);
}

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
export function installFetch(
  backend,
  discovery,
  {
    globalObj = globalThis,
    mintIdempotencyKey = defaultMintIdempotencyKey,
    outboundTargets = null,
  } = {},
) {
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
    // Pattern-aware target selection (docs/targeting.md): exact host key, else
    // the most specific matching host/URL pattern key, else the bare host.
    // `outboundTargets` is `compileOutboundMatchers(policy)`, compiled once at
    // install time; with none installed this is exactly the old `resolveTarget`.
    const target = resolvePolicyTarget(outboundTargets, {
      method,
      hostname,
      scheme: parsed.protocol.replace(/:$/, ""),
      port: parsed.port ? Number(parsed.port) : null,
      path: parsed.pathname,
    });
    const op = `${method} ${hostname}${parsed.pathname}`;
    const idemHeader = readIdempotencyHeader(backend, target);
    // Injection (contracts/adapter-pack.md "Idempotency-key injection"): mint
    // once, before the first attempt, and set it on the normalized `headers`
    // — the same object every retry attempt below forwards as `attemptInit`,
    // so the SAME key rides every attempt (rule 2).
    const injectedKey = resolveIdempotencyInjection(method, headers, idemHeader, mintIdempotencyKey);
    if (injectedKey !== null) headers.set(idemHeader, injectedKey);
    // A call is only retried if it is BOTH idempotent by method/header/injection
    // AND its body can be re-sent on a retry: an unbuffered stream body is
    // consumed once, so a call carrying one is downgraded to non-idempotent
    // (Level 0: can't wrap safely → observed, not retried). In-memory bodies
    // (string/bytes) are re-sent unchanged on each attempt.
    const idempotent =
      (injectedKey !== null || isIdempotent(method, headers, idemHeader)) && isBodyRetrySafe(input, body);
    // args_hash (cache/journal key material): idempotent GET, plus the documented
    // LLM-POST dev-cache exception — cross-language parity with the Python twin's
    // derive_args_hash (a cacheable POST is not thereby made retryable).
    const hash = deriveArgsHash(target, method, parsed.href, body);
    const request = { v: 1, target, op, idempotent, args_hash: hash };

    // Per-attempt timeout is enforced only for idempotent calls: a timeout we
    // impose becomes a new thrown error, and we must never inject one into a
    // non-idempotent success path.
    const timeoutMs = idempotent ? durationMs(backend.layer(target, "timeout")) : null;

    // Only a call the core may actually cache needs its body buffered into the
    // payload envelope (for a cross-call / cross-run replay). Everything else
    // sends a cheap status/headers envelope and hands back the LIVE response.
    const cacheCfg = backend.layer(target, "cache");
    const cacheable = hash != null && isTable(cacheCfg) && cacheCfg.ttl !== undefined;

    // Live objects kept side-band so the core payload can stay JSON: the winning
    // ok response, the last superseded transient (5xx/429), and the last thrown
    // transport error. The response we hand back keeps its body; superseded
    // transients are cancelled so retries never leave an undrained body.
    let heldOk = null;
    let heldTransient = null;
    let heldErr = null;

    const started = performance.now();
    const outcome = await backend.execute(request, async () => {
      const { signal, cancel } = withTimeout(init?.signal, timeoutMs);
      // An injected key must actually reach the wire: forward the normalized
      // `headers` (which carries it) as the attempt's headers. Every other
      // call is untouched — `init` (or the caller's Request) flows through
      // exactly as before, preserving byte-for-byte transparency.
      const attemptInit = { ...init, ...(injectedKey !== null ? { headers } : {}), ...(signal ? { signal } : {}) };
      try {
        const resp = await original.call(this, input, attemptInit);
        heldErr = null;
        if (isTransientStatus(resp.status)) {
          if (heldTransient && heldTransient !== resp) cancelBody(heldTransient);
          heldTransient = resp;
          return {
            status: "error",
            class: "http",
            http_status: resp.status,
            retry_after_ms: parseRetryAfter(resp.headers.get("retry-after")),
            message: `HTTP ${resp.status}`,
          };
        }
        cancelBody(heldTransient); // a good response supersedes any held transient
        heldTransient = null;
        heldOk = resp;
        return { status: "ok", payload: await responseEnvelope(resp, { withBody: cacheable }) };
      } catch (err) {
        heldErr = err;
        return { status: "error", class: classifyThrow(err), message: err?.message ?? String(err) };
      } finally {
        cancel();
      }
    });

    discovery?.observe(target, outcome, performance.now() - started);

    if (outcome.result === "ok") {
      // A cache hit (in-process or, under the persistent journal, across runs)
      // rebuilds the response from the envelope; a live call returns the real,
      // unchanged response object (byte-transparency, DX invariant).
      if (outcome.from_cache) return attachOutcome(rebuildResponse(outcome.payload), outcome);
      return attachOutcome(heldOk, outcome);
    }

    // Error: the LAST attempt's live object decides delivery. A thrown transport
    // error (heldErr set on the final attempt) is re-thrown unchanged; otherwise
    // the last real 5xx/429 response is returned unchanged (retries exhausted).
    if (heldErr instanceof Error) {
      cancelBody(heldTransient); // dangling transient before a thrown transport error
      throw attachOutcome(heldErr, outcome);
    }
    if (heldTransient) return attachOutcome(heldTransient, outcome);
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

/**
 * Whether a retried attempt can safely re-send this request's body. In-memory
 * bodies (string, bytes, URLSearchParams, Blob) are re-sent unchanged on each
 * attempt; an unbuffered ReadableStream body — or a `Request` input carrying a
 * live body — can be consumed only once, so a call using one is NOT retried
 * (Level 0: do nothing if it can't be wrapped safely; the caller sees a single
 * attempt, observed-not-retried). No body → trivially safe.
 */
function isBodyRetrySafe(input, body) {
  // A Request instance streams its own body; re-sending it would fail on retry.
  if (typeof Request !== "undefined" && input instanceof Request && input.body != null) return false;
  if (body == null) return true;
  if (typeof body === "string") return true;
  if (typeof URLSearchParams !== "undefined" && body instanceof URLSearchParams) return true;
  if (typeof Blob !== "undefined" && body instanceof Blob) return true;
  if (body instanceof ArrayBuffer || ArrayBuffer.isView(body)) return true;
  // ReadableStream, FormData, async iterables and any other opaque body: treat
  // as unbuffered and conservatively do not retry (a corrupted resend is worse
  // than a single observed attempt).
  return false;
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
