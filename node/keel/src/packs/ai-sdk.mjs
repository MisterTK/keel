/**
 * Vercel AI SDK pack — a `LanguageModelV2` middleware (DX spec §4.2: "first-class
 * middleware (wrapLanguageModel) — the cleanest seam of any framework; the
 * adapter is thin and safe"). The one place where "zero code changes" is spent
 * on the framework's OWN blessed extension point: the user plugs Keel in via
 *
 *     import { wrapLanguageModel } from "ai";
 *     import { keelMiddleware } from "keel/ai-sdk";
 *     const model = wrapLanguageModel({ model: base, middleware: keelMiddleware() });
 *
 * and changes nothing else. The real `ai` package is NOT a dependency of Keel;
 * this module only implements the middleware SHAPE (`wrapGenerate`/`wrapStream`)
 * that `wrapLanguageModel` calls. The shape is pinned in
 * ../../fixtures/ai-sdk-model.d.ts (mirrors ai@5.0.0) and contract-tested against
 * a minimal fake model.
 *
 * Semantics:
 *   - target = `llm:<provider>` from the wrapped model's provider id (the base
 *     segment before any `.`, e.g. "openai.chat" → llm:openai). These are the
 *     same `llm:` targets the fetch host map produces, so they inherit the
 *     `[defaults.llm]` pack (retry on 429/5xx/timeout, provider-aware backoff via
 *     Retry-After, dev cache).
 *   - wrapGenerate: the doGenerate() call is the effect; a thrown provider error
 *     is classified (429/5xx → retry per policy, Retry-After honored). The
 *     result is dev-cache-eligible (args_hash over the call params).
 *   - wrapStream: resilience wraps stream ESTABLISHMENT (the doStream() call),
 *     NOT chunks. Once the stream is established it is returned UNCHANGED and its
 *     chunks flow through untouched — Keel never buffers, observes, or retries
 *     mid-stream (a live stream is not replayable). args_hash is null for streams
 *     so the dev cache never applies to them.
 *   - on final failure the original provider error propagates unchanged, with a
 *     non-enumerable `keelOutcome` attached (DX invariant 5), exactly like the
 *     fetch seam.
 */

import { createHash } from "node:crypto";
import { getBackend, getDiscovery, attachOutcome } from "../runtime.mjs";
import { KeelError } from "../engine.mjs";
import { parseRetryAfter } from "../judge.mjs";

/** Derive `llm:<provider>` from a model's provider id (base segment). */
export function providerTarget(model) {
  const provider = typeof model?.provider === "string" && model.provider ? model.provider : "unknown";
  const base = provider.split(".")[0] || "unknown";
  return `llm:${base}`;
}

/** Stable JSON with sorted object keys, so equivalent params hash identically. */
function stableStringify(value) {
  return JSON.stringify(value, (_key, v) => {
    if (v && typeof v === "object" && !Array.isArray(v)) {
      const sorted = {};
      for (const k of Object.keys(v).sort()) sorted[k] = v[k];
      return sorted;
    }
    return v;
  });
}

function hashParams(params) {
  let s;
  try {
    s = stableStringify(params);
  } catch {
    return null; // non-serializable params → not cacheable
  }
  if (s == null) return null;
  return createHash("sha256").update(s).digest("hex");
}

/**
 * A JSON-safe deep clone for the core `payload` (cache-store material only). The
 * native core serde-round-trips the payload, so a live object — a `ReadableStream`,
 * an `Error`, a provider `response` with a `Date`, or anything holding a function —
 * cannot cross it. We clone what CAN be stored and keep the live result side-band;
 * a non-serializable result simply becomes uncacheable (the live object is still
 * delivered by identity on the success path). Returns `null` on failure.
 */
function jsonClone(v) {
  try {
    const s = JSON.stringify(v);
    return s === undefined ? null : JSON.parse(s);
  } catch {
    return null;
  }
}

function headerGet(headers, name) {
  if (!headers) return undefined;
  if (typeof headers.get === "function") return headers.get(name);
  const lower = name.toLowerCase();
  for (const k of Object.keys(headers)) if (k.toLowerCase() === lower) return headers[k];
  return undefined;
}

/**
 * Classify a thrown provider error into a core error class. AI SDK provider
 * errors (APICallError) expose `statusCode` + `responseHeaders`; we map HTTP
 * status the same way the fetch seam does so retry policy behaves identically.
 */
export function classifyModelError(err) {
  const status = err?.statusCode ?? err?.status ?? err?.response?.status;
  if (typeof status === "number") {
    const retryAfter = parseRetryAfter(headerGet(err?.responseHeaders ?? err?.headers, "retry-after"));
    const out = { class: "http", http_status: status, message: err?.message ?? `HTTP ${status}` };
    if (retryAfter !== undefined) out.retry_after_ms = retryAfter;
    return out;
  }
  const name = err?.name;
  // Split what the fetch seam splits (judge.mjs classifyThrow): a deadline abort
  // is a `timeout` (retryable by default), but a CALLER's AbortController fires
  // an `AbortError` — that is `cancelled` (excluded from the default retry.on →
  // immediately terminal, so an aborted generate/stream propagates at once
  // instead of grinding through the backoff schedule).
  if (name === "TimeoutError") return { class: "timeout", message: err?.message };
  if (name === "AbortError") return { class: "cancelled", message: err?.message };
  if (name === "TypeError") return { class: "conn", message: err?.message };
  return { class: "other", message: err?.message ?? String(err) };
}

function settle(outcome, target, discovery, latencyMs, held) {
  discovery?.observe(target, outcome, latencyMs);
  if (outcome.result === "ok") {
    // Live call → the real provider result unchanged (identity + a live stream
    // preserved, byte-transparent). Cache hit → the round-tripped JSON payload
    // replayed by the core (in-process, or across runs under the journal).
    const value = held.haveResult && !outcome.from_cache ? held.liveResult : outcome.payload;
    return attachOutcome(value, outcome);
  }
  // Terminal failure: re-raise the ORIGINAL provider error, held side-band (DX
  // invariant 5) — the core carries no live error object.
  if (held.liveErr instanceof Error) throw attachOutcome(held.liveErr, outcome);
  if (held.liveErr !== undefined) throw held.liveErr;
  const e = new KeelError(outcome.error?.code ?? "KEEL-E040", outcome.error?.message ?? "keel llm failure");
  throw attachOutcome(e, outcome);
}

/**
 * Build the Keel `LanguageModelV2` middleware. Zero-config: uses the backend
 * installed by the hook. `options.backend`/`options.discovery` allow injection
 * for embedding and tests. When no backend is active (Keel disabled / not
 * installed) the middleware is a transparent pass-through (DX invariant 2).
 */
export function keelMiddleware(options = {}) {
  const backendOf = () => options.backend ?? getBackend();
  const discoveryOf = () => options.discovery ?? getDiscovery();

  const run = async ({ effect, params, model, kind, cacheable }) => {
    const backend = backendOf();
    if (!backend) return effect(); // disabled: transparent pass-through
    const target = providerTarget(model);
    const op = `${kind} ${target}${model?.modelId ? ` ${model.modelId}` : ""}`;
    const request = {
      v: 1,
      target,
      op,
      idempotent: true,
      args_hash: cacheable ? hashParams(params) : null,
    };
    const started = performance.now();
    // Live result/error held side-band so the core payload stays JSON: the native
    // core cannot round-trip a live stream/Date/Error. On the success path we
    // hand back the real object (identity + streams preserved); only a cache HIT
    // uses the JSON payload. A stream result must NEVER cross the core, so a
    // non-cacheable effect (streams) sends no payload at all.
    const held = { liveResult: undefined, haveResult: false, liveErr: undefined };
    const outcome = await backend.execute(request, async () => {
      try {
        held.liveResult = await effect();
        held.haveResult = true;
        return { status: "ok", payload: cacheable ? jsonClone(held.liveResult) : null };
      } catch (err) {
        held.liveErr = err;
        return { status: "error", ...classifyModelError(err) };
      }
    });
    return settle(outcome, target, discoveryOf(), performance.now() - started, held);
  };

  return {
    // generate: dev-cache-eligible; the whole doGenerate result is the payload.
    wrapGenerate: ({ doGenerate, params, model }) =>
      run({ effect: doGenerate, params, model, kind: "generate", cacheable: true }),
    // stream: resilience on ESTABLISHMENT only; chunks pass through unchanged.
    wrapStream: ({ doStream, params, model }) =>
      run({ effect: doStream, params, model, kind: "stream", cacheable: false }),
  };
}
