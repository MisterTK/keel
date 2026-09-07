/**
 * The exit-time console summary — `[telemetry].console` (design spec Part A).
 *
 *     keel ▸ 47 calls · absorbed 3 rate limits · 2 retries succeeded · 1 breaker trip · 4 calls unprotected
 *            keel report --open for the full picture
 *
 * Counters are fed by `discovery.observe()` — the one seam every intercepted
 * call passes through, and the only place that knows whether the target was
 * wrapped — then formatted once from `installExitFlush`'s `flush`. Wording is
 * pinned by conformance/console_summary/*.json, shared with the Python front
 * end (parity: identical counts → identical bytes).
 */

import { existsSync } from "node:fs";
import { delimiter, join } from "node:path";

export const PREFIX = "keel ▸ ";
/** Continuation lines align under the text after the prefix (7 columns). */
export const INDENT = " ".repeat(PREFIX.length);

const KEYS = ["calls", "throttled", "retries_succeeded", "breaker_trips", "cache_hits", "not_retried", "unprotected"];

export function createSummary() {
  const c = Object.fromEntries(KEYS.map((k) => [k, 0]));
  return {
    /** Fold one call's outcome in. Mirrors discovery.mjs's classification,
     *  except `retries_succeeded` (success-only, narrower than `retries`). */
    observe(outcome, wrapped) {
      if (outcome == null) return;
      const attempts = Number.isFinite(outcome.attempts) ? outcome.attempts : 0;
      const ok = outcome.result === "ok";
      const fromCache = outcome.from_cache === true;
      const code = outcome.error?.code;
      c.calls += 1;
      if (outcome.throttled) c.throttled += 1;
      if (ok && !fromCache && attempts > 1) c.retries_succeeded += 1;
      if (code === "KEEL-E012") c.breaker_trips += 1; // breaker fast-fail (and LLM budget block, by design)
      if (ok && fromCache) c.cache_hits += 1;
      if (code === "KEEL-E014") c.not_retried += 1; // observed, not retried
      if (!wrapped) c.unprotected += 1;
    },
    counts() {
      return { ...c };
    },
  };
}

function n(count, singular, plural) {
  return `${count} ${count === 1 ? singular : plural}`;
}

/** The two lines (with trailing newlines), or "" when nothing was intercepted. */
export function formatSummary(counts, keelOnPath) {
  const calls = counts.calls ?? 0;
  if (calls === 0) return "";
  const segments = [n(calls, "call", "calls")];
  if (counts.throttled) segments.push(`absorbed ${n(counts.throttled, "rate limit", "rate limits")}`);
  if (counts.retries_succeeded) segments.push(`${n(counts.retries_succeeded, "retry", "retries")} succeeded`);
  if (counts.breaker_trips) segments.push(n(counts.breaker_trips, "breaker trip", "breaker trips"));
  if (counts.cache_hits) segments.push(`${counts.cache_hits} served from cache`);
  if (counts.not_retried) segments.push(`${n(counts.not_retried, "failure", "failures")} not retried`);
  if (counts.unprotected) segments.push(`${n(counts.unprotected, "call", "calls")} unprotected`);
  const command = keelOnPath ? "keel report --open" : "uvx --from keelrun-cli keel report --open";
  return `${PREFIX}${segments.join(" · ")}\n${INDENT}${command} for the full picture\n`;
}

/** Whether the `keel` CLI binary is on PATH — decides which bridge line prints. */
export function keelOnPath(env = process.env) {
  const names = process.platform === "win32" ? ["keel.exe", "keel.cmd", "keel"] : ["keel"];
  for (const dir of (env.PATH ?? "").split(delimiter)) {
    if (!dir) continue;
    for (const name of names) {
      try {
        if (existsSync(join(dir, name))) return true;
      } catch {
        /* unreadable PATH entry — skip */
      }
    }
  }
  return false;
}
