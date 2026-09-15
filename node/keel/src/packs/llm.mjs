/**
 * `llm:` provider defaults pack (adapter-pack contract, contracts/adapter-pack.md
 * + contracts/stubs/adapter-pack.ts). This is the Node twin of the Python LLM
 * pack (Task 11 items 1–2): the SAME defaults fragment, the SAME merge
 * semantics, and the SAME dev-cache resolution. Both front ends must agree, so
 * the rules here are the cross-language contract — change them in lockstep.
 *
 * The pack itself carries ZERO resilience logic (adapter-pack rule 3): it only
 * (a) declares the generic `[defaults.llm]` fragment (merged UNDER user config)
 * and (b) resolves the dev-mode response cache into a concrete cache directive
 * the core understands. All retry/backoff/breaker/cache behavior runs in the
 * core (AsyncEngine today, the native core after the swap).
 *
 * Dev-cache resolution (DX spec §4.1; defaults.toml `cache = { mode = "dev" }`):
 *   - the dev cache is a dev-loop response cache: identical prompt+params replay
 *     from cache during development (10× faster iteration, ~0 API spend);
 *   - it is INERT when `KEEL_ENV=prod` (production never serves stale replays);
 *   - the core caches on `cache.ttl`, so off-prod we resolve `mode = "dev"` to a
 *     concrete session-length ttl; in prod we drop the cache layer entirely.
 * The dev-cache lifetime (`DEV_CACHE_TTL`) is an in-process implementation
 * detail — the CONTRACT is the semantics (active off-prod, inert in prod, keyed
 * by target+args_hash), which is what the Python twin's counters also assert.
 */

import { llmDefaults } from "../defaults.mjs";

/** Dev-loop cache lifetime. The value is not itself a parity contract (the
 *  semantics are); it only needs to outlast a dev session. */
export const DEV_CACHE_TTL = "24h";

function isTable(v) {
  return v !== null && typeof v === "object" && !Array.isArray(v);
}

/** Environment variables that identify a serverless/container platform
 *  (Cloud Run services and jobs, Lambda, Azure Functions and App Service).
 *  Their presence means "production" for the dev cache when KEEL_ENV is
 *  unset — replaying an LLM response from a cache in a deployed container is
 *  never the dev loop the cache exists for (field report 2026-09-15, F0/F6). */
export const SERVERLESS_MARKERS = [
  "K_SERVICE",
  "K_REVISION",
  "CLOUD_RUN_JOB",
  "AWS_LAMBDA_FUNCTION_NAME",
  "FUNCTIONS_WORKER_RUNTIME",
  "WEBSITE_SITE_NAME",
];

/** The first present, non-blank serverless marker variable, or null. */
export function serverlessMarker(env = process.env) {
  for (const name of SERVERLESS_MARKERS) {
    if (String(env?.[name] ?? "").trim()) return name;
  }
  return null;
}

/**
 * WHY the LLM dev cache is off in this process, as a machine-readable token —
 * `"KEEL_ENV"` when an explicit `KEEL_ENV=prod` demoted it, the serverless
 * marker's variable name when a container did, or `null` when the cache is
 * genuinely ON.
 *
 * Single source of truth for `isProd` (below) AND for the activation line's
 * `dev_cache_off` field, so the field can never claim something the cache
 * resolution does not do: a field literally named `dev_cache_off` reporting
 * null is a positive claim that the cache is on, and `KEEL_ENV=prod` is a
 * reachable production configuration where that would be a lie. Twin of
 * Python's `dev_cache_off_reason`; keep identical.
 *
 * Only the two declarations Keel actually understands short-circuit the
 * marker: `prod` (off) and `dev` (on — someone saying "this IS my dev loop"
 * may have one inside a container). Every other value, blank included, falls
 * through to the serverless marker: `KEEL_ENV=staging` on Cloud Run must not
 * silently re-arm the dev cache, which is the 2026-09-15 outage class itself
 * (spec §2: a detected serverless environment counts as `KEEL_ENV=prod` for
 * dev-cache resolution).
 */
export function devCacheOffReason(env = process.env) {
  // Trim + lowercase before comparing — cross-language parity with the Python
  // twin's `.strip().lower()`.
  const keelEnv = String(env?.KEEL_ENV ?? "").trim().toLowerCase();
  if (keelEnv === "prod") return "KEEL_ENV";
  if (keelEnv === "dev") return null; // an explicit dev declaration beats a marker
  return serverlessMarker(env);
}

function isProd(env) {
  return devCacheOffReason(env) !== null;
}

/** The `llm:` adapter pack — the four uniform operations (adapter-pack.md). */
export const llmPack = Object.freeze({
  /**
   * The `llm:` pack is a semantic pack, not a library shim: it applies whenever
   * an intercepted call resolves to an `llm:<provider>` target (the fetch host
   * map in judge.mjs, or the AI-SDK middleware). It always "matches" — there is
   * no external library version to pin — so confidence is `pinned`.
   */
  detect() {
    return { matched: true, name: "llm", confidence: "pinned" };
  },
  /**
   * No monkey-patched seam of its own: `llm:` targets are produced by other
   * seams (global fetch → host map; the AI-SDK middleware; MCP is separate).
   */
  seams() {
    return [];
  },
  targets() {
    return [
      {
        pattern: "llm:<provider>",
        kind: "llm",
        idempotencyRule:
          "LLM generate/stream calls are treated as retryable (idempotent) so 429/5xx/timeout retry per the pack",
        argsHashRule:
          "null for a GET state query (issue #76) and for a streaming generate call (:streamGenerateContent path or \"stream\": true body — issue #84); sha256 over the (key-sorted) call params for a non-streaming generate call — (method, url, canonicalized JSON body) at the fetch seam, the call params themselves at the AI-SDK middleware seam (dev-cache replay key); null for an unbuffered/streaming body (a live stream is not cache-replayable)",
      },
    ];
  },
  /** Policy fragment merged UNDER user config: the generic `[defaults.llm]`. */
  defaults() {
    return { defaults: { llm: llmDefaults() } };
  },
});

/**
 * Resolve dev-mode caches in a policy into concrete cache directives the core
 * understands, honoring `KEEL_ENV`. Walks every `cache` layer that could apply
 * (`defaults.llm`, `defaults.outbound`, and each `[target."…"]`):
 *
 *   - `cache = { mode = "dev" }`  → off-prod: `cache = { ttl = "<DEV_CACHE_TTL>" }`
 *                                   (preserving an explicit user ttl if present),
 *                                   plus `scope = "persistent"` when `persistent`
 *                                   is set (native + journal) so identical prompts
 *                                   replay across RUNS, not just within one;
 *                                 → prod:    the cache layer is removed (inert).
 *   - any other cache layer is left exactly as-is.
 *
 * Returns a NEW policy; the input is never mutated.
 */
export function resolveDevCache(policy, env = process.env, { persistent = false } = {}) {
  const prod = isProd(env);
  const out = structuredClone(isTable(policy) ? policy : {});

  const resolveOn = (owner) => {
    if (!isTable(owner) || !isTable(owner.cache) || owner.cache.mode !== "dev") return;
    if (prod) {
      delete owner.cache; // dev cache is inert in prod
      return;
    }
    const next = { ...owner.cache };
    delete next.mode;
    if (next.ttl === undefined) next.ttl = DEV_CACHE_TTL;
    if (persistent && next.scope === undefined) next.scope = "persistent";
    owner.cache = next;
  };

  if (isTable(out.defaults)) {
    resolveOn(out.defaults.llm);
    resolveOn(out.defaults.outbound);
  }
  if (isTable(out.target)) {
    for (const t of Object.values(out.target)) resolveOn(t);
  }
  return out;
}
