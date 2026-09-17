/**
 * Runtime self-diagnosis of the cache-poll pathology (WS9, issue #78): N
 * consecutive cache hits for ONE identical call, spaced like a status poll,
 * almost always means a status-poll loop is being fed a replayed "still
 * running" response. Keel names it once per (target, argsHash) instead of
 * letting the app hang to its own deadline (field incidents 2026-08-30 and
 * 2026-09-15). No new error code — a warning line with a `code` in its JSON
 * twin; `keel explain` content lands with poll v2 (#93). Twin of
 * `python/keel/src/keel/_cachepoll.py`.
 */

/**
 * Cap on tracked (target, argsHash) runs and on the fired set, in BOTH
 * languages (Python twin: `_cachepoll.py`'s `MAX_RUNS`/`MAX_FIRED`) — this
 * detector lives inside other people's long-running server processes, and a
 * server making many DISTINCT cached calls (an LLM app with varying prompts
 * is precisely our audience) must not accumulate one entry per distinct call
 * for the life of the process. Named constants, not magic numbers.
 */
export const MAX_RUNS = 1024;
export const MAX_FIRED = 1024;

/**
 * Per-`(target, argsHash)` run tracker. A `from_cache` outcome increments
 * the run's hit count; any other outcome for the same key resets it — "N
 * consecutive" means consecutive. Fires once per key per process when
 * `hits >= minHits` AND the run's span (`last - first`) is at least
 * `minSpanS` — the span rule is what tells a status poll (hits spaced
 * seconds apart) apart from a test suite replaying one prompt in a burst.
 * Never fires when `argsHash` is null/falsy.
 *
 * No lock, unlike the Python twin: Node is single-threaded, and every
 * caller of `observe` (the fetch seam's hop loop, and every pack's
 * `discovery.observe`) resumes on this same event-loop thread — there is no
 * concurrent-mutation hazard here to guard against.
 */
export function createCachePollDetector({
  now = () => performance.now() / 1000,
  minHits = 5,
  // KEEL_CACHEPOLL_MIN_SPAN_S is UNSTABLE — test-only, read once at
  // construction, so the acceptance test can shrink the ~minutes-long real
  // pathology into a ~25s run without changing production defaults.
  minSpanS = Number(process.env.KEEL_CACHEPOLL_MIN_SPAN_S ?? 20),
  onSuspect = null,
} = {}) {
  const runs = new Map(); // key -> { hits, first, last }
  const fired = new Set(); // insertion-ordered

  /** Drop the tracked run with the OLDEST `last` timestamp. A key that has
   *  not been touched recently is by definition not a live poll run, so
   *  evicting it cannot lose a detection in progress. */
  function evictOldestRun() {
    let oldestKey = null;
    let oldestLast = Infinity;
    for (const [k, r] of runs) {
      if (r.last < oldestLast) {
        oldestLast = r.last;
        oldestKey = k;
      }
    }
    if (oldestKey !== null) runs.delete(oldestKey);
  }

  /** Simple insertion-order eviction once the fired set is full. Re-firing a
   *  key after it has been evicted from a full fired set is acceptable, and
   *  strictly better than unbounded growth. */
  function markFired(key) {
    if (fired.size >= MAX_FIRED) fired.delete(fired.values().next().value);
    fired.add(key);
  }

  return {
    observe(target, argsHash, outcome) {
      if (!argsHash) return false;
      const key = `${target} ${argsHash}`;
      if (!outcome?.from_cache) {
        runs.delete(key);
        return false;
      }
      const t = now();
      let run = runs.get(key);
      if (!run) {
        if (runs.size >= MAX_RUNS) evictOldestRun();
        runs.set(key, { hits: 1, first: t, last: t });
        return false;
      }
      run.hits += 1;
      run.last = t;
      if (fired.has(key) || run.hits < minHits || run.last - run.first < minSpanS) {
        return false;
      }
      markFired(key);
      onSuspect?.(target, run.hits, Math.trunc(run.last - run.first));
      return true;
    },
    /** Test-only introspection (not part of the detector's public
     *  contract): current tracked-run and fired-set sizes, so eviction
     *  tests can assert the caps hold without reaching into module-private
     *  state. */
    _debugSizes() {
      return { runs: runs.size, fired: fired.size };
    },
  };
}

/**
 * The text + `KEEL_LOG_FORMAT=json` twin for one detector firing (#78/WS9),
 * pulled out as its own testable builder (mirroring `deploy.mjs`'s
 * `ephemeralJournalWarning`) rather than inlined at `bootstrap.mjs`'s call
 * site. `severity` is `WARNING` (#130): this line names an operator-visible
 * pathology — a status poll silently fed a replayed response — and must be
 * as filterable as the activation and refusal lines are. Python twin:
 * `_cachepoll.py`'s `cache_poll_suspect_warning`.
 */
export function cachePollSuspectWarning(target, hits, spanS, version) {
  const text =
    `keel ▸ warning: ${target} served ${hits} consecutive cache hits for one identical ` +
    `call over ${spanS}s — if this is a status poll, set cache = ` +
    `{ mode = "off" } on that target ` +
    `— or give the status route its own poll policy (README: Poll)\n`;
  const obj = {
    keel: "warning",
    code: "cache-poll-suspect",
    target,
    hits,
    span_s: spanS,
    severity: "WARNING",
    version,
  };
  return [text, obj];
}
