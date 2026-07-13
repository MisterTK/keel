/**
 * LLM budget caps + model fallback chains (DX spec §4.1) — FRONT-END
 * enforcement of the two frozen `llm:` target policy knobs
 * (`contracts/policy.schema.json` `$defs/budget` / `targetPolicy.fallback`):
 *
 *   budget   = "$5/run"                              → per-run spend cap
 *   fallback = ["gpt-4o-mini", "claude-haiku-4.5"]    → model fallback chain
 *
 * DESIGN (decided, not re-litigated here): both knobs are enforced entirely at
 * the front-end `llm:` seams (this module + `fetch.mjs` + `packs/ai-sdk.mjs`),
 * NOT in the core/FFI/stub. The core has no concept of "budget" or "model" — it
 * executes an opaque effect and applies retry/breaker/cache/rate to typed
 * AttemptResults. Reusing KEEL-E012 (breaker-open) for a budget trip is a
 * front-end CHOICE (dx-spec: "budget = \"$5/run\" -> breaker trips"), not a new
 * core error path: this module synthesizes an Outcome shaped exactly like the
 * core's own breaker-open Outcome (same fields `discovery.observe` reads), so
 * `keel status`/`.keel/discovery.db` accounting (`breaker_opens`, `failures`)
 * picks up a budget block for free, with zero crates/ changes.
 *
 * Cross-language parity note: this is the Node twin of
 * `python/keel/src/keel/adapters/_llm_policy.py`. Both MUST stay in lockstep —
 * same price table, same usage-normalization key order, same fallback trigger
 * rule, same v0.1 rewrite limitation (below).
 *
 * ---------------------------------------------------------------------------
 * Per-run spend accounting
 * ---------------------------------------------------------------------------
 * "Per run" = this process's lifetime (Tier 1 has no run-id concept; matches
 * the dev-cache's session-scoped philosophy). The ledger is a plain in-memory
 * map, reset only by `resetLlmBudgets()` (tests) or process exit. A budget cap
 * is READ from the effective policy at every call (`backend.layer(target,
 * "budget")`), so changing `keel.toml` and reconfiguring changes the cap live;
 * spend already recorded this run is never retroactively erased.
 *
 * Usage is read from the provider's OWN response — a DELIBERATE, narrowly
 * scoped exception to the adapter packs' "never read the response body" rule,
 * gated strictly on the target actually carrying a `budget` (an operator who
 * never sets `budget` gets byte-transparent bodies exactly as before). Prices
 * are an ESTIMATE (a maintained table below) — provider pricing drifts; this
 * is not a billing-accurate ledger, it is a resilience guardrail.
 *
 * ---------------------------------------------------------------------------
 * Fallback re-dispatch
 * ---------------------------------------------------------------------------
 * Fallback triggers on any TERMINAL call failure for the target EXCEPT
 * breaker-open (KEEL-E012) — which also covers our own budget synthesis above,
 * matching dx-spec's "NOT on budget exhaustion": chasing a fallback into an
 * open breaker (or an exhausted budget) would defeat the point of failing
 * fast. It does NOT trigger on success, and it does not trigger on a cache hit
 * (nothing failed).
 *
 * v0.1 LIMITATION (honest, documented, not silently pretended away): model
 * rewriting at the generic HTTP/fetch seam can only change the MODEL on the
 * SAME host/endpoint the original request already targeted — it rewrites
 * either a JSON body's top-level `model` field (OpenAI/Anthropic chat/messages
 * shape) or a Google `.../models/<model>:generate...` URL path segment. It
 * CANNOT construct a request for a genuinely different provider (different
 * auth headers, different endpoint, different request/response shape). A
 * `fallback` chain that names a model from a different provider than the one
 * that failed is sent to the SAME provider with that (unrecognized) model
 * name, which the provider will typically reject with its own 4xx — a safe,
 * honest failure, not silent data loss, but not the cross-provider magic the
 * dx-spec's `fallback = ["gemini-2.5-pro", "claude-sonnet-4.5"]` example
 * suggests either. True cross-provider fallback needs a seam that already
 * knows how to build a request per provider; `packs/ai-sdk.mjs`'s
 * `keelMiddleware({ models })` option supports that (the caller supplies a
 * real `LanguageModelV2` instance per fallback name).
 */

// --- budget: parsing, pricing, ledger ----------------------------------------

/** Parse the frozen `budget` grammar (`^\$[0-9]+(\.[0-9]+)?/run$`) to a cap in
 *  CENTS, or `null` when absent/malformed (never throws — an unparseable
 *  budget string is a policy-validation concern, not this module's). */
export function parseBudgetCents(spec) {
  if (typeof spec !== "string") return null;
  const m = /^\$([0-9]+(?:\.[0-9]+)?)\/run$/.exec(spec.trim());
  if (!m) return null;
  const dollars = Number(m[1]);
  return Number.isFinite(dollars) ? Math.round(dollars * 100) : null;
}

/**
 * USD per 1,000,000 tokens, by model-name PREFIX (longest-prefix-wins so e.g.
 * "gpt-4o-mini" doesn't fall through to a shorter "gpt-4o" entry). THIS TABLE
 * DRIFTS — provider pricing changes independently of Keel releases; treat it
 * as a maintained ESTIMATE for budget enforcement, not a billing source of
 * truth. Update alongside the Python twin (`_llm_policy.py`) when it goes
 * stale.
 */
export const PRICE_TABLE_USD_PER_MILLION = Object.freeze({
  "gpt-4o-mini": { input: 0.15, output: 0.6 },
  "gpt-4o": { input: 2.5, output: 10 },
  "gpt-4.1-mini": { input: 0.4, output: 1.6 },
  "gpt-4.1": { input: 2, output: 8 },
  "gpt-5-mini": { input: 0.25, output: 2 },
  "gpt-5": { input: 1.25, output: 10 },
  "claude-haiku-4.5": { input: 1, output: 5 },
  "claude-sonnet-4.5": { input: 3, output: 15 },
  "claude-opus-4.5": { input: 15, output: 75 },
  "gemini-2.5-flash": { input: 0.3, output: 2.5 },
  "gemini-2.5-pro": { input: 1.25, output: 10 },
});

/** Used for any model not in the table above, so budget enforcement degrades
 *  gracefully instead of silently never counting an unrecognized model.
 *  Deliberately conservative (high) — an unknown model trips the cap sooner
 *  rather than risking silent overspend. */
export const DEFAULT_PRICE_USD_PER_MILLION = Object.freeze({ input: 10, output: 30 });

function priceFor(model) {
  const name = String(model ?? "").toLowerCase();
  let best = null;
  for (const [prefix, price] of Object.entries(PRICE_TABLE_USD_PER_MILLION)) {
    if (name.startsWith(prefix) && (best === null || prefix.length > best.prefix.length)) {
      best = { prefix, price };
    }
  }
  return best ? best.price : DEFAULT_PRICE_USD_PER_MILLION;
}

/** Estimated USD cost of one call's usage, given the model name (may be
 *  `null`/unknown — falls back to `DEFAULT_PRICE_USD_PER_MILLION`). */
export function estimateCostUsd(model, usage) {
  if (!usage) return 0;
  const price = priceFor(model);
  const inCost = ((usage.inputTokens ?? 0) / 1_000_000) * price.input;
  const outCost = ((usage.outputTokens ?? 0) / 1_000_000) * price.output;
  return inCost + outCost;
}

/**
 * Normalize a provider response body (or an AI-SDK `usage` object) into
 * `{inputTokens, outputTokens}`, or `null` when no recognizable usage is
 * present. Handles, in priority order:
 *   - OpenAI chat/completions:      `{ usage: { prompt_tokens, completion_tokens } }`
 *   - Anthropic messages:           `{ usage: { input_tokens, output_tokens } }`
 *   - Google generateContent:       `{ usageMetadata: { promptTokenCount, candidatesTokenCount } }`
 *   - AI SDK v2 `LanguageModelV2Usage` (passed directly, already unwrapped):
 *     `{ inputTokens, outputTokens }` (v5) or `{ promptTokens, completionTokens }` (v4).
 */
export function normalizeUsage(obj) {
  if (!obj || typeof obj !== "object") return null;
  const u = obj.usage ?? obj.usageMetadata ?? obj;
  if (!u || typeof u !== "object") return null;
  const input = u.input_tokens ?? u.inputTokens ?? u.prompt_tokens ?? u.promptTokens ?? u.promptTokenCount;
  const output =
    u.output_tokens ?? u.outputTokens ?? u.completion_tokens ?? u.completionTokens ?? u.candidatesTokenCount;
  if (input == null && output == null) return null;
  return { inputTokens: Number(input) || 0, outputTokens: Number(output) || 0 };
}

// Per-process ("per-run") spend ledger, keyed by target. Cents (integers) to
// avoid float drift across many small additions.
const ledger = new Map();

/** Record `usd` (an estimated cost) against `target`'s running spend. */
export function recordSpend(target, usd) {
  if (!(usd > 0)) return;
  ledger.set(target, (ledger.get(target) ?? 0) + Math.round(usd * 100));
}

/** Cents spent against `target` so far this run. */
export function spentCents(target) {
  return ledger.get(target) ?? 0;
}

/** Test-only: clear all recorded spend (a fresh "run"). */
export function resetLlmBudgets() {
  ledger.clear();
}

/** The what/why/next message for a budget-exceeded block (DX invariant: first
 *  line human, KEEL-E0NN code, concrete next step). */
export function budgetMessage(target, capCents, spent) {
  const cap = (capCents / 100).toFixed(2);
  const spentStr = (spent / 100).toFixed(2);
  return (
    `LLM budget cap $${cap}/run for ${target} is exhausted (spent $${spentStr} this run); ` +
    `the call was blocked before dispatch, like an open circuit breaker. Raise ` +
    `target."${target}".budget (or defaults.llm.budget) in keel.toml, or reduce request volume.`
  );
}

/** An Outcome envelope shaped exactly like the core/stub's own breaker-open
 *  Outcome, so `discovery.observe` (breaker_opens / failures accounting) and
 *  the fetch/ai-sdk seams' existing delivery logic treat a budget block
 *  identically to a real breaker trip. */
export function budgetBlockedOutcome(message) {
  return {
    v: 1,
    result: "error",
    attempts: 0,
    from_cache: false,
    waits_ms: [],
    throttled: false,
    throttle_wait_ms: 0,
    breaker: "open",
    error: { code: "KEEL-E012", class: "other", message },
  };
}

// --- fallback: model derivation + rewriting ----------------------------------

function parseJsonBody(body) {
  if (typeof body !== "string") return null;
  try {
    return JSON.parse(body);
  } catch {
    return null;
  }
}

const GENAI_MODEL_URL = /\/models\/([^/:?]+):/;

/** The model name a request targets, or `null` when it can't be determined
 *  (see the module doc's v0.1 rewrite limitation). */
export function deriveRequestModel(url, body) {
  const parsed = parseJsonBody(body);
  if (parsed && typeof parsed.model === "string") return parsed.model;
  const m = GENAI_MODEL_URL.exec(String(url ?? ""));
  return m ? decodeURIComponent(m[1]) : null;
}

/**
 * Rewrite a request to target `newModel` for the next fallback hop. Returns
 * `{ url, body }`, or `null` when the request shape is unrecognized (fallback
 * then stops there; the CURRENT failure is delivered to the caller — see the
 * module doc's v0.1 limitation on cross-provider fallback).
 */
export function rewriteModel(url, body, newModel) {
  const parsed = parseJsonBody(body);
  if (parsed && typeof parsed.model === "string") {
    parsed.model = newModel;
    return { url, body: JSON.stringify(parsed) };
  }
  if (GENAI_MODEL_URL.test(String(url ?? ""))) {
    return { url: url.replace(GENAI_MODEL_URL, `/models/${encodeURIComponent(newModel)}:`), body };
  }
  return null;
}

/** Whether a terminal call failure should chase the next model in a fallback
 *  chain. Excludes breaker-open (KEEL-E012) — real trips AND our own budget
 *  synthesis both fail fast on purpose; chasing a fallback would defeat that. */
export function shouldFallback(error) {
  return !!error && error.code !== "KEEL-E012";
}
