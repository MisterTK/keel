/**
 * AsyncEngine: the runtime resilience backend for the Node front end.
 *
 * The `keel-core-stub` (node/keel-core-stub) is a SYNCHRONOUS decision engine
 * on a virtual clock — it drives attempts by calling `effect(attempt)` and
 * inspecting the returned value inline. A real front end intercepts async I/O
 * (fetch, async functions), so the effect cannot be synchronous. Until the
 * native async core lands ("the swap", Task 14) the Node backend runs an async
 * port of the stub's `execute`.
 *
 * The decision logic here is a faithful, verbatim-derived port of
 * `keel-core-stub`'s layer chain (cache → rate → breaker → retry), condition
 * matching, terminal codes (KEEL-E010/E012/E014/E015), and Outcome envelope.
 * Parity is not left to trust: `test/engine-parity.test.mjs` runs identical
 * scripted attempt sequences through BOTH this engine (virtual clock) and the
 * real `KeelCoreStub` and asserts byte-identical outcomes. Change the semantics
 * in one place and that test fails.
 *
 * Deliberate additions over the stub, documented and safe:
 *   - clocks are injectable so tests are deterministic (virtualClock) while
 *     production uses realClock (real backoff sleeps, real wall time).
 *   - `configure` is delegated to a `KeelCoreStub` instance so validation and
 *     its KEEL-E001 field-path messages are shared, never re-derived.
 */

// Vendored copy of node/keel-core-stub/index.mjs so the published package is
// self-contained (npm pack cannot ship files above the package root). It is
// byte-identical to the source — enforced by test/vendored-stub-parity.test.mjs
// and scripts/check-release-metadata.sh; refresh with scripts/sync-vendored.sh.
import { KeelCoreStub, KeelError, ENVELOPE_VERSION } from "./vendor/keel-core-stub/index.mjs";

export { KeelError, ENVELOPE_VERSION };

// --- constants (mirror keel-core-stub) ---------------------------------------
const DEFAULT_SCHEDULE = { base: 200, factor: 2, cap: 30_000 };
const DEFAULT_ATTEMPTS = 3;
const DEFAULT_ON = ["conn", "timeout", "429", "5xx"];
const DEFAULT_BREAKER_FAILURES = 5;
const DEFAULT_BREAKER_COOLDOWN_MS = 15_000;

const DURATION_MULT = { ms: 1, s: 1_000, m: 60_000, h: 3_600_000 };
const RATE_WINDOW = { s: 1_000, sec: 1_000, min: 60_000, h: 3_600_000, hour: 3_600_000 };

// --- ported pure helpers (verbatim from keel-core-stub) ----------------------
function parseDuration(s) {
  const m = /^(\d+)(ms|s|m|h)$/.exec(String(s).trim());
  return m ? Number(m[1]) * DURATION_MULT[m[2]] : null;
}

function parseRate(s) {
  const m = /^(\d+)\/(s|sec|min|h|hour)$/.exec(String(s).trim());
  if (!m) return null;
  const limit = Number(m[1]);
  return limit >= 1 ? { limit, windowMs: RATE_WINDOW[m[2]] } : null;
}

function parseSchedule(s) {
  s = String(s).trim();
  if (s.startsWith("exp(") && s.endsWith(")")) {
    const parts = s.slice(4, -1).split(",").map((p) => p.trim());
    if (parts.length < 2) return null;
    const base = parseDuration(parts[0]);
    if (base === null || !parts[1].startsWith("x")) return null;
    const factor = Number(parts[1].slice(1));
    if (!Number.isFinite(factor)) return null;
    let cap = Number.MAX_SAFE_INTEGER;
    for (const p of parts.slice(2)) {
      if (p.startsWith("max ")) {
        cap = parseDuration(p.slice(4));
        if (cap === null) return null;
      } else if (p !== "jitter") {
        return null;
      }
    }
    return { base, factor, cap };
  }
  if (s.startsWith("fixed(") && s.endsWith(")")) {
    const d = parseDuration(s.slice(6, -1));
    return d === null ? null : { base: d, factor: 1, cap: d };
  }
  return null;
}

function scheduleWait(sched, attempt) {
  return Math.round(Math.min(sched.base * sched.factor ** (attempt - 1), sched.cap));
}

function conditionMatches(cond, cls, httpStatus) {
  if (["conn", "timeout", "cancelled", "other"].includes(cond)) return cond === cls;
  if (cls !== "http" || httpStatus == null) return false;
  if (cond === "4xx") return httpStatus >= 400 && httpStatus <= 499;
  if (cond === "5xx") return httpStatus >= 500 && httpStatus <= 599;
  return /^\d{3}$/.test(cond) && Number(cond) === httpStatus;
}

function isTable(v) {
  return v !== null && typeof v === "object" && !Array.isArray(v);
}

// --- clocks ------------------------------------------------------------------
/** Production clock: wall time; backoff waits actually sleep. */
export function realClock() {
  return {
    now: () => Date.now(),
    sleep: (ms) => (ms > 0 ? new Promise((r) => setTimeout(r, ms)) : Promise.resolve()),
    advance() {},
  };
}

/** Deterministic clock for tests/parity: waits advance a virtual counter. */
export function virtualClock() {
  let t = 0;
  return {
    now: () => t,
    sleep: async (ms) => {
      t += ms;
    },
    advance: (ms) => {
      t += ms;
    },
  };
}

/**
 * Async port of KeelCoreStub. `kind` identifies the backend in diagnostics.
 * `execute(request, effect)` takes an ASYNC effect: `(attempt) => Promise<AttemptResult>`.
 */
export class AsyncEngine {
  kind = "node-stub";
  #policy = {};
  #validator = new KeelCoreStub();
  #clock;
  #traceSeq = 0;
  #breakers = new Map();
  #rateWindows = new Map();
  #cache = new Map();
  #metrics = new Map();

  constructor(clock = realClock()) {
    this.#clock = clock;
  }

  /** Validate + store policy. Validation (and its KEEL-E001 field paths) is
   *  delegated to KeelCoreStub so the two never diverge. Throws KeelError. */
  configure(policy) {
    this.#validator.configure(policy); // throws KeelError on invalid policy
    this.#policy = policy;
  }

  /** Public layer resolution (front-end judgments consult this, e.g. to find a
   *  target's idempotency header). Identical rule to KeelCoreStub#layer. */
  layer(target, key) {
    const t = this.#policy.target;
    if (isTable(t) && isTable(t[target]) && t[target][key] !== undefined) return t[target][key];
    const defaults = this.#policy.defaults ?? {};
    if (target.startsWith("llm:") && isTable(defaults.llm) && defaults.llm[key] !== undefined)
      return defaults.llm[key];
    return isTable(defaults.outbound) ? defaults.outbound[key] : undefined;
  }

  advanceClock(ms) {
    this.#clock.advance(ms);
  }

  #met(target) {
    if (!this.#metrics.has(target))
      this.#metrics.set(target, {
        calls: 0,
        attempts: 0,
        retries: 0,
        successes: 0,
        failures: 0,
        cache_hits: 0,
        throttled: 0,
      });
    return this.#metrics.get(target);
  }

  #breakerState(target) {
    const b = this.#breakers.get(target);
    return b && b.openUntil !== null && this.#clock.now() < b.openUntil ? "open" : "closed";
  }

  async execute(request, effect) {
    const target = request.target;
    const op = request.op ?? target;
    const m = this.#met(target);
    m.calls += 1;
    this.#traceSeq += 1;
    const out = {
      v: ENVELOPE_VERSION,
      result: "error",
      attempts: 0,
      from_cache: false,
      waits_ms: [],
      throttled: false,
      throttle_wait_ms: 0,
      breaker: "closed",
      trace_id: `t-${String(this.#traceSeq).padStart(6, "0")}`,
    };

    if (request.v !== ENVELOPE_VERSION) {
      out.error = {
        code: "KEEL-E004",
        class: "other",
        message: `unsupported envelope version ${request.v}`,
      };
      m.failures += 1;
      return out;
    }

    const retry = this.layer(target, "retry");
    const breakerCfg = this.layer(target, "breaker");
    const rate = this.layer(target, "rate");
    const cacheCfg = this.layer(target, "cache");
    const cacheTtl =
      isTable(cacheCfg) && cacheCfg.ttl !== undefined ? parseDuration(cacheCfg.ttl) : null;

    // cache (outermost layer)
    const argsHash = request.args_hash;
    const cacheKey = cacheTtl !== null && argsHash ? `${target}#${argsHash}` : null;
    if (cacheKey && this.#cache.has(cacheKey)) {
      const { expires, payload } = this.#cache.get(cacheKey);
      if (this.#clock.now() < expires) {
        m.cache_hits += 1;
        m.successes += 1;
        out.result = "ok";
        out.payload = payload;
        out.from_cache = true;
        out.breaker = this.#breakerState(target);
        return out;
      }
    }

    // rate limiter (fixed windows on the clock)
    if (typeof rate === "string") {
      const { limit, windowMs } = parseRate(rate);
      const now = this.#clock.now();
      const w = Math.floor(now / windowMs);
      if (!this.#rateWindows.has(target)) this.#rateWindows.set(target, { window: w, count: 0 });
      const cell = this.#rateWindows.get(target);
      if (cell.window !== w) {
        cell.window = w;
        cell.count = 0;
      }
      if (cell.count >= limit) {
        const next = (cell.window + 1) * windowMs;
        out.throttle_wait_ms = next - now;
        out.throttled = true;
        await this.#clock.sleep(next - now);
        cell.window = Math.floor(this.#clock.now() / windowMs);
        cell.count = 0;
        m.throttled += 1;
      }
      cell.count += 1;
    }

    // breaker check (observes post-retry call outcomes)
    let halfOpen = false;
    if (isTable(breakerCfg)) {
      if (!this.#breakers.has(target))
        this.#breakers.set(target, { consecutive: 0, openUntil: null, opens: 0 });
      const b = this.#breakers.get(target);
      if (b.openUntil !== null) {
        if (this.#clock.now() < b.openUntil) {
          out.error = {
            code: "KEEL-E012",
            class: "other",
            message: `breaker OPEN for ${target}: failed fast, call not attempted`,
          };
          out.breaker = "open";
          m.failures += 1;
          return out;
        }
        halfOpen = true;
      }
    }

    // retry loop
    let maxAttempts = 1;
    let schedule = DEFAULT_SCHEDULE;
    let on = DEFAULT_ON;
    if (isTable(retry)) {
      maxAttempts = retry.attempts ?? DEFAULT_ATTEMPTS;
      schedule = retry.schedule !== undefined ? parseSchedule(retry.schedule) : DEFAULT_SCHEDULE;
      on = retry.on ?? DEFAULT_ON;
    }

    let terminal = null;
    for (let attempt = 1; attempt <= maxAttempts; attempt++) {
      out.attempts = attempt;
      m.attempts += 1;
      const res = await effect(attempt);
      if (res.status === "ok") {
        m.successes += 1;
        if (isTable(breakerCfg)) {
          const b = this.#breakers.get(target);
          b.consecutive = 0;
          b.openUntil = null;
        }
        if (cacheKey && cacheTtl !== null)
          this.#cache.set(cacheKey, { expires: this.#clock.now() + cacheTtl, payload: res.payload });
        out.result = "ok";
        out.payload = res.payload;
        out.breaker = this.#breakerState(target);
        return out;
      }

      const cls = res.class ?? "other";
      const httpStatus = res.http_status;
      const message = res.message ?? "";
      const retryable = on.some((c) => conditionMatches(c, cls, httpStatus));
      let code = null;
      if (!retryable) code = "KEEL-E015";
      else if (attempt === maxAttempts) code = "KEEL-E010";
      else if (!request.idempotent) code = "KEEL-E014";
      if (code) {
        const detail = httpStatus != null ? `${cls} ${httpStatus}` : cls;
        let msg;
        if (code === "KEEL-E010")
          msg = `${op} failed ${attempt}/${maxAttempts} attempts (last: ${detail}). ${message}`;
        else if (code === "KEEL-E014")
          msg = `${op} failed (${detail}). Not retried: call is not idempotent — observed, not retried. ${message}`;
        else msg = `${op} failed (${detail}); error class is not retryable per policy. ${message}`;
        terminal = { code, class: cls, message: msg.trimEnd() };
        if (httpStatus != null) terminal.http_status = httpStatus;
        if (res.original !== undefined) terminal.original = res.original;
        break;
      }
      let wait = scheduleWait(schedule, attempt);
      if (res.retry_after_ms != null) wait = Math.max(wait, res.retry_after_ms);
      out.waits_ms.push(wait);
      await this.#clock.sleep(wait);
      m.retries += 1;
    }

    // terminal failure
    m.failures += 1;
    if (isTable(breakerCfg)) {
      const failures = breakerCfg.failures ?? DEFAULT_BREAKER_FAILURES;
      const cooldown =
        breakerCfg.cooldown !== undefined
          ? parseDuration(breakerCfg.cooldown)
          : DEFAULT_BREAKER_COOLDOWN_MS;
      const b = this.#breakers.get(target);
      if (halfOpen) {
        b.openUntil = this.#clock.now() + cooldown;
        b.opens += 1;
        b.consecutive = 0;
      } else {
        b.consecutive += 1;
        if (b.consecutive >= failures) {
          b.openUntil = this.#clock.now() + cooldown;
          b.opens += 1;
          b.consecutive = 0;
        }
      }
    }
    out.error = terminal;
    out.breaker = this.#breakerState(target);
    return out;
  }

  /** Deterministic report; same contractual shape as KeelCoreStub.report()
   *  ({v, clock_ms, targets}) — clock_ms comes from the injected clock. */
  report() {
    const targets = {};
    for (const name of [...this.#metrics.keys()].sort()) {
      const m = this.#metrics.get(name);
      const b = this.#breakers.get(name);
      targets[name] = {
        attempts: m.attempts,
        breaker_opens: b ? b.opens : 0,
        breaker_state: this.#breakerState(name),
        cache_hits: m.cache_hits,
        calls: m.calls,
        failures: m.failures,
        retries: m.retries,
        successes: m.successes,
        throttled: m.throttled,
      };
    }
    return { v: 1, clock_ms: this.#clock.now(), targets };
  }
}
