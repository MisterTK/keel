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
 *   - an LLM POST (llm:* target) additionally honors `budget` (per-run spend
 *     cap, blocks the call before dispatch once exceeded) and `fallback`
 *     (re-dispatch to the next model in the chain on a qualifying terminal
 *     failure) — see `llm-policy.mjs` for the full design and its documented
 *     v0.1 limitations. Both are no-ops (zero overhead, zero body reads) for
 *     any target that does not configure them.
 */

import {
  normalizeRequest,
  resolveTarget,
  isIdempotent,
  deriveArgsHash,
  parseRetryAfter,
  classifyThrow,
  isTransientStatus,
  responseEnvelope,
  rebuildResponse,
} from "./judge.mjs";
import { attachOutcome } from "./runtime.mjs";
import { KeelError } from "./engine.mjs";
import {
  parseBudgetCents,
  spentCents,
  recordSpend,
  estimateCostUsd,
  normalizeUsage,
  budgetMessage,
  budgetBlockedOutcome,
  deriveRequestModel,
  rewriteModel,
  shouldFallback,
} from "./llm-policy.mjs";

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
    const idemHeader = readIdempotencyHeader(backend, target);
    // A call is only retried if it is BOTH idempotent by method/header AND its
    // body can be re-sent on a retry: an unbuffered stream body is consumed once,
    // so a call carrying one is downgraded to non-idempotent (Level 0: can't wrap
    // safely → observed, not retried). In-memory bodies (string/bytes) are
    // re-sent unchanged on each attempt (the same init.body is passed through).
    const idempotent = isIdempotent(method, headers, idemHeader) && isBodyRetrySafe(input, body);

    // Per-attempt timeout is enforced only for idempotent calls: a timeout we
    // impose becomes a new thrown error, and we must never inject one into a
    // non-idempotent success path.
    const timeoutMs = idempotent ? durationMs(backend.layer(target, "timeout")) : null;

    // Only a call the core may actually cache needs its body buffered into the
    // payload envelope (for a cross-call / cross-run replay). Everything else
    // sends a cheap status/headers envelope and hands back the LIVE response.
    const cacheCfg = backend.layer(target, "cache");

    // --- LLM budget + fallback (llm-policy.mjs) — llm:* POST targets only ---
    const isLlmGenerate = target.startsWith("llm:") && method === "POST";
    const capCents = isLlmGenerate ? parseBudgetCents(backend.layer(target, "budget")) : null;
    const fallbackCfgRaw = isLlmGenerate ? backend.layer(target, "fallback") : undefined;
    const fallbackChain = Array.isArray(fallbackCfgRaw) ? fallbackCfgRaw.filter((m) => typeof m === "string" && m) : [];
    const trackUsage = capCents !== null;

    if (capCents !== null && spentCents(target) >= capCents) {
      const message = budgetMessage(target, capCents, spentCents(target));
      const blocked = budgetBlockedOutcome(message);
      discovery?.observe(target, blocked, 0);
      throw attachOutcome(new KeelError("KEEL-E012", message), blocked);
    }

    // Headers/init reused for any fallback hop beyond the first (a plain URL +
    // init call — no library-specific Request-object surgery needed for fetch).
    const initForHops = { ...(init ?? {}), method, headers };

    let hopUrl = parsed.href;
    let hopBody = body;
    let hopIndex = 0; // 0 = the ORIGINAL request, as given; >0 = fallback[hopIndex - 1]
    let outcome;
    let heldOk = null;
    let heldTransient = null;
    let heldErr = null;

    // Bounded by `fallbackChain.length` (checked before each extra hop below).
    while (true) {
      const hopParsed = new URL(hopUrl);
      const op = `${method} ${hostname}${hopParsed.pathname}`;
      const hash = deriveArgsHash(target, method, hopParsed.href, hopBody);
      const request = { v: 1, target, op, idempotent, args_hash: hash };
      const cacheable = hash != null && isTable(cacheCfg) && cacheCfg.ttl !== undefined;

      heldOk = null;
      heldTransient = null;
      heldErr = null;
      const started = performance.now();
      outcome = await backend.execute(request, async () => {
        const { signal, cancel } = withTimeout(init?.signal, timeoutMs);
        const attemptInit =
          hopIndex === 0
            ? signal
              ? { ...init, signal }
              : init
            : { ...initForHops, body: hopBody, ...(signal ? { signal } : {}) };
        try {
          const resp = await original.call(this, hopIndex === 0 ? input : hopUrl, attemptInit);
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
          return { status: "ok", payload: await responseEnvelope(resp, { withBody: cacheable || trackUsage }) };
        } catch (err) {
          heldErr = err;
          return { status: "error", class: classifyThrow(err), message: err?.message ?? String(err) };
        } finally {
          cancel();
        }
      });

      discovery?.observe(target, outcome, performance.now() - started);

      if (outcome.result === "ok") {
        if (trackUsage && !outcome.from_cache) recordLlmSpend(target, hopUrl, hopBody, outcome.payload);
        break;
      }

      // Terminal failure: chase the next model in the fallback chain when one
      // is configured and the failure qualifies (see llm-policy.mjs). Rewriting
      // may fail (unrecognized request shape) — then we stop and deliver THIS
      // failure, honestly, rather than pretend a hop happened.
      if (hopIndex >= fallbackChain.length || !shouldFallback(outcome.error)) break;
      const rewritten = rewriteModel(hopUrl, hopBody, fallbackChain[hopIndex]);
      if (!rewritten) break;
      hopUrl = rewritten.url;
      hopBody = rewritten.body;
      hopIndex += 1;
    }

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

/** Extract usage from a live (non-cache) response envelope's buffered JSON body
 *  (see llm-policy.mjs's documented body-reading exception) and record its
 *  estimated cost against the target's per-run ledger. Best-effort: a
 *  non-JSON or usage-less body silently records nothing (never breaks the
 *  call over an accounting miss). */
function recordLlmSpend(target, requestUrl, requestBody, payload) {
  const b64 = payload?.body_b64;
  if (typeof b64 !== "string") return;
  let parsed;
  try {
    parsed = JSON.parse(Buffer.from(b64, "base64").toString("utf8"));
  } catch {
    return;
  }
  const usage = normalizeUsage(parsed);
  if (!usage) return;
  const model = deriveRequestModel(requestUrl, requestBody);
  recordSpend(target, estimateCostUsd(model, usage));
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
