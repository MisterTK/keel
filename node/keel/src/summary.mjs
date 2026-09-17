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
import { dumpsLine } from "./log.mjs";

export const PREFIX = "keel ▸ ";
/** Continuation lines align under the text after the prefix (7 columns). */
export const INDENT = " ".repeat(PREFIX.length);

const KEYS = ["calls", "throttled", "retries_succeeded", "breaker_trips", "cache_hits", "not_retried", "unprotected"];

export function createSummary() {
  const c = Object.fromEntries(KEYS.map((k) => [k, 0]));
  const byTarget = new Map();
  // JSON-summary-only (WS9, #78) — never printed in the text form, so it is
  // not one of KEYS.
  let cachePollSuspects = 0;
  return {
    /** Fold one call's outcome in. Mirrors discovery.mjs's classification,
     *  except `retries_succeeded` (success-only, narrower than `retries`).
     *  `target` is optional and, alongside an unwrapped call, feeds
     *  `unprotectedByTarget()` — the attribution behind the exit summary's
     *  "N calls unprotected (…)" breakdown (#96). */
    observe(outcome, wrapped, target = null) {
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
      if (!wrapped) {
        c.unprotected += 1;
        if (target) byTarget.set(target, (byTarget.get(target) ?? 0) + 1);
      }
    },
    counts() {
      return { ...c };
    },
    unprotectedByTarget() {
      return Object.fromEntries(byTarget);
    },
    /** Called once per fired detector key (WS9, #78) — JSON summary only. */
    noteCachePollSuspect() {
      cachePollSuspects += 1;
    },
    cachePollSuspects() {
      return cachePollSuspects;
    },
  };
}

function n(count, singular, plural) {
  return `${count} ${count === 1 ? singular : plural}`;
}

/**
 * The `unprotected` segment: a bare count, or — when a per-target breakdown
 * is supplied — the count plus a parenthetical naming the top three targets
 * (count desc, then name asc), with a `+{k} others` tail when more than
 * three targets contributed (#96).
 */
function unprotectedSegment(count, byTarget) {
  const seg = `${n(count, "call", "calls")} unprotected`;
  const entries = byTarget ? Object.entries(byTarget) : [];
  if (entries.length === 0) return seg;
  const ranked = entries.sort((a, b) => (b[1] - a[1]) || (a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : 0));
  let top = ranked.slice(0, 3).map(([t, c]) => `${t} ${c}`).join(", ");
  const rest = ranked.length - 3;
  if (rest > 0) top += `, +${n(rest, "other", "others")}`;
  return `${seg} (${top})`;
}

/** The two lines (with trailing newlines), or "" when nothing was intercepted. */
export function formatSummary(counts, keelOnPath, byTarget = null) {
  const calls = counts.calls ?? 0;
  if (calls === 0) return "";
  const segments = [n(calls, "call", "calls")];
  if (counts.throttled) segments.push(`absorbed ${n(counts.throttled, "rate limit", "rate limits")}`);
  if (counts.retries_succeeded) segments.push(`${n(counts.retries_succeeded, "retry", "retries")} succeeded`);
  if (counts.breaker_trips) segments.push(n(counts.breaker_trips, "breaker trip", "breaker trips"));
  if (counts.cache_hits) segments.push(`${counts.cache_hits} served from cache`);
  if (counts.not_retried) segments.push(`${n(counts.not_retried, "failure", "failures")} not retried`);
  if (counts.unprotected) segments.push(unprotectedSegment(counts.unprotected, byTarget));
  const command = keelOnPath ? "keel report --open" : "uvx --from keelrun-cli keel report --open";
  return `${PREFIX}${segments.join(" · ")}\n${INDENT}${command} for the full picture\n`;
}

/**
 * The `KEEL_LOG_FORMAT=json` twin of `formatSummary`: one line, sorted keys,
 * no spaces. Unlike the text form it prints even at zero calls — in a
 * container "Keel activated and intercepted nothing" is itself the evidence
 * the outage post-mortem needed. Pinned by conformance/console_summary_json/,
 * which the Python front end reads too (identical bytes, both languages).
 * `unprotected_by_target` carries the FULL map (every target, sorted keys,
 * `{}` when none) — the text line only ever names the top three (#96).
 * `cache_poll_suspects` (WS9, #78) is JSON-summary-only — never printed in
 * the text form, so it is not one of KEYS.
 */
export function formatSummaryJson(counts, meta, byTarget = null, cachePollSuspects = 0) {
  const obj = {};
  for (const k of KEYS) obj[k] = Number(counts?.[k] ?? 0);
  obj.unprotected_by_target = { ...(byTarget ?? {}) };
  obj.cache_poll_suspects = Number(cachePollSuspects ?? 0);
  obj.severity = "INFO";
  return dumpsLine({ ...obj, keel: "summary", ...meta });
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
