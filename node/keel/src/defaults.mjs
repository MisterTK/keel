/**
 * Level 0 embedded smart-defaults pack (DX spec §1) + policy-merge helper.
 *
 * The layers below MIRROR contracts/defaults.toml verbatim (that file is the
 * frozen source of truth). We embed rather than read the file so the package is
 * self-contained (zero runtime deps, works when installed from npm). Drift
 * against the contract is caught by test/defaults-parity.test.mjs, which parses
 * contracts/defaults.toml with our TOML parser and asserts deep equality with
 * `level0Defaults()` when the repo file is present.
 *
 * The Level 0 hard rules (never change success-path semantics; never retry
 * non-idempotent calls; do nothing if it can't be wrapped safely) are BEHAVIOR,
 * not config — enforced in the front-end judgments, not expressible here.
 */

function isTable(v) {
  return v !== null && typeof v === "object" && !Array.isArray(v);
}

/** `[defaults.outbound]` — any intercepted network call. */
export function outboundDefaults() {
  return {
    timeout: "30s",
    retry: {
      attempts: 3,
      schedule: "exp(200ms, x2, max 30s, jitter)",
      on: ["conn", "timeout", "429", "5xx"],
    },
    breaker: { failures: 5, cooldown: "15s" },
  };
}

/** `[defaults.llm]` — any `llm:*` target; the generic LLM pack layer. */
export function llmDefaults() {
  return {
    timeout: "120s",
    retry: {
      attempts: 6,
      schedule: "exp(500ms, x2, max 60s, jitter)",
      on: ["conn", "timeout", "429", "5xx"],
    },
    breaker: { failures: 5, cooldown: "30s" },
    cache: { mode: "dev" },
  };
}

export function level0Defaults() {
  return { defaults: { outbound: outboundDefaults(), llm: llmDefaults() } };
}

/**
 * Merge the embedded pack defaults UNDER a user policy (adapter-pack contract:
 * "defaults() → policy fragment merged UNDER user config"; defaults.toml: "a
 * user keel.toml is layered on top of these defaults").
 *
 * Merge granularity mirrors the engine's own layer resolution, which is
 * per-KEY (a target's `retry`/`breaker`/`cache`/`timeout` is used wholesale):
 * so the merge fills in any `defaults.outbound`/`defaults.llm` key the user did
 * NOT set, and a user-set key replaces the pack default for that key wholesale.
 * Target tables are left untouched — the engine already resolves target →
 * defaults.llm → defaults.outbound precedence per key at execute time.
 *
 * Returns a NEW policy object; the input is never mutated. Idempotent: applying
 * it to `level0Defaults()` returns an equivalent policy.
 */
export function applyPackDefaults(policy) {
  const out = structuredClone(isTable(policy) ? policy : {});
  const userDefaults = isTable(out.defaults) ? out.defaults : {};
  const userOutbound = isTable(userDefaults.outbound) ? userDefaults.outbound : {};
  const userLlm = isTable(userDefaults.llm) ? userDefaults.llm : {};
  out.defaults = {
    ...userDefaults,
    outbound: { ...outboundDefaults(), ...userOutbound },
    llm: { ...llmDefaults(), ...userLlm },
  };
  return out;
}
