/**
 * Level 0 embedded smart-defaults pack (DX spec §1).
 *
 * This is the policy applied when no keel.toml is present. It MIRRORS
 * contracts/defaults.toml verbatim (that file is the frozen source of truth).
 * We embed rather than read the file so the package is self-contained (zero
 * runtime deps, works when installed from npm). Drift against the contract is
 * caught by test/defaults-parity.test.mjs, which parses contracts/defaults.toml
 * with our TOML parser and asserts deep equality when the repo file is present.
 *
 * The Level 0 hard rules (never change success-path semantics; never retry
 * non-idempotent calls; do nothing if it can't be wrapped safely) are BEHAVIOR,
 * not config — enforced in the front-end judgments, not expressible here.
 */

export function level0Defaults() {
  return {
    defaults: {
      outbound: {
        timeout: "30s",
        retry: {
          attempts: 3,
          schedule: "exp(200ms, x2, max 30s, jitter)",
          on: ["conn", "timeout", "429", "5xx"],
        },
        breaker: { failures: 5, cooldown: "15s" },
      },
      llm: {
        timeout: "120s",
        retry: {
          attempts: 6,
          schedule: "exp(500ms, x2, max 60s, jitter)",
          on: ["conn", "timeout", "429", "5xx"],
        },
        breaker: { failures: 5, cooldown: "30s" },
        cache: { mode: "dev" },
      },
    },
  };
}
