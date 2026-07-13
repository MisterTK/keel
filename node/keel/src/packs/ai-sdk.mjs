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
 *   - `budget` (per-run spend cap) and `fallback` (model fallback chain) — see
 *     `llm-policy.mjs` for the shared design. This seam is the ONE place a
 *     fallback hop can be a genuinely DIFFERENT PROVIDER (not just a same-host
 *     model swap like the fetch seam manages): pass `keelMiddleware({ models })`
 *     with a map of fallback name → an already-constructed `LanguageModelV2`
 *     instance for that model, and a qualifying failure re-dispatches to it.
 *     Without a matching entry in `models`, fallback for that hop is a no-op
 *     (the chain stops there and the current failure is delivered) — an
 *     unresolvable name is not silently pretended away.
 */

import { createHash } from "node:crypto";
import { getBackend, getDiscovery, attachOutcome } from "../runtime.mjs";
import { KeelError } from "../engine.mjs";
import { parseRetryAfter } from "../judge.mjs";
import {
  parseBudgetCents,
  spentCents,
  recordSpend,
  estimateCostUsd,
  normalizeUsage,
  budgetMessage,
  budgetBlockedOutcome,
  shouldFallback,
} from "../llm-policy.mjs";

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

function deliver(outcome, held) {
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
 * `options.models` maps a `fallback` chain entry (by name) to an alternate
 * `LanguageModelV2` instance — see the module doc's fallback section.
 */
export function keelMiddleware(options = {}) {
  const backendOf = () => options.backend ?? getBackend();
  const discoveryOf = () => options.discovery ?? getDiscovery();
  const modelsOf = () => (options.models && typeof options.models === "object" ? options.models : {});

  const run = async ({ effect: primaryEffect, params, model: primaryModel, kind, cacheable }) => {
    const backend = backendOf();
    if (!backend) return primaryEffect(); // disabled: transparent pass-through

    let currentModel = primaryModel;
    let currentEffect = primaryEffect;
    let hopIndex = 0;
    let target;
    let outcome;
    let held;

    // Bounded by the primary target's `fallback` chain length (re-checked below).
    for (;;) {
      target = providerTarget(currentModel);

      if (hopIndex === 0) {
        // Budget is gated ONLY on the PRIMARY target, once, before any dispatch
        // (parity with fetch.mjs's design) — a fallback hop's own target is not
        // itself budget-gated in v0.1 (documented scope; see llm-policy.mjs).
        const capCents = parseBudgetCents(backend.layer(target, "budget"));
        if (capCents !== null && spentCents(target) >= capCents) {
          const message = budgetMessage(target, capCents, spentCents(target));
          const blocked = budgetBlockedOutcome(message);
          discoveryOf()?.observe(target, blocked, 0);
          throw attachOutcome(new KeelError("KEEL-E012", message), blocked);
        }
      }

      const capForUsage = parseBudgetCents(backend.layer(target, "budget"));
      const op = `${kind} ${target}${currentModel?.modelId ? ` ${currentModel.modelId}` : ""}`;
      const request = {
        v: 1,
        target,
        op,
        idempotent: true,
        args_hash: cacheable ? hashParams(params) : null,
      };
      held = { liveResult: undefined, haveResult: false, liveErr: undefined };
      const started = performance.now();
      outcome = await backend.execute(request, async () => {
        try {
          held.liveResult = await currentEffect();
          held.haveResult = true;
          return { status: "ok", payload: cacheable ? jsonClone(held.liveResult) : null };
        } catch (err) {
          held.liveErr = err;
          return { status: "error", ...classifyModelError(err) };
        }
      });
      discoveryOf()?.observe(target, outcome, performance.now() - started);

      if (outcome.result === "ok") {
        if (capForUsage !== null && held.haveResult && !outcome.from_cache) {
          const usage = normalizeUsage(held.liveResult);
          if (usage) recordSpend(target, estimateCostUsd(currentModel?.modelId, usage));
        }
        break;
      }

      const fallbackCfg = backend.layer(target, "fallback");
      const chain = Array.isArray(fallbackCfg) ? fallbackCfg.filter((m) => typeof m === "string" && m) : [];
      if (hopIndex >= chain.length || !shouldFallback(outcome.error)) break;
      const altModel = modelsOf()[chain[hopIndex]];
      const methodName = kind === "generate" ? "doGenerate" : "doStream";
      if (!altModel || typeof altModel[methodName] !== "function") break; // unresolvable — stop, honestly
      currentModel = altModel;
      currentEffect = () => altModel[methodName](params);
      hopIndex += 1;
    }

    return deliver(outcome, held);
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
