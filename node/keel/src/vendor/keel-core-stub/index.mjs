/**
 * keel-core-stub: an in-memory fake of the keel-core surface (Node form).
 *
 * Mirrors crates/keel-core-stub semantics exactly; the shared specification
 * lives in conformance/README.md. Envelopes are plain objects shaped like the
 * serde types in contracts/core_api.rs.
 *
 * Simplifications (deliberate, documented): virtual clock (waits recorded,
 * not slept), no jitter, exact-match target resolution with defaults.llm /
 * defaults.outbound fallback, `timeout` validated but not enforced.
 *
 * Bit-identical to the real core (parity rule, not a simplification): the
 * breaker's count mode *and* rate mode (window + failure_rate + min_calls),
 * and the rate limiter's token bucket (burst = the rate's limit, continuous
 * refill).
 */

export const ENVELOPE_VERSION = 1;

// A single unbounded exp segment; see parseSchedule() for the segment shape.
const DEFAULT_SCHEDULE = [{ primary: { base: 200, factor: 2, cap: 30_000 }, upToMs: null }];
const DEFAULT_ATTEMPTS = 3;
const DEFAULT_ON = ["conn", "timeout", "429", "5xx"];
const DEFAULT_BREAKER_FAILURES = 5;
const DEFAULT_BREAKER_COOLDOWN_MS = 15_000;

const DURATION_MULT = { ms: 1, s: 1_000, m: 60_000, h: 3_600_000 };
const RATE_WINDOW = { s: 1_000, sec: 1_000, min: 60_000, h: 3_600_000, hour: 3_600_000 };

export class KeelError extends Error {
  constructor(code, message) {
    super(`${code}: ${message}`);
    this.code = code;
  }
}

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

/** A schedule primary: exp(base, xF[, max D][, jitter]) or fixed(D). */
function parsePrimary(s) {
  s = s.trim();
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

function primaryWait(primary, localAttempt) {
  return Math.round(Math.min(primary.base * primary.factor ** (localAttempt - 1), primary.cap));
}

/**
 * Full grammar (contracts/schedule-grammar.ebnf): one or more `andThen`-
 * separated segments, each a primary with an optional cumulative-wait `upTo`
 * bound. Returns `[{ primary, upToMs }, ...]` or null. Semantics pinned in
 * conformance/README.md ("Schedule algebra"): every segment but the last
 * must be bounded, and the last never is — both degenerate shapes are
 * rejected here so a schedule is always a total mapping attempt -> wait.
 */
function parseSchedule(s) {
  const tokens = String(s).trim().split(/\s+/).filter((t) => t.length > 0);
  if (tokens.length === 0) return null;
  const groups = [[]];
  for (const token of tokens) {
    if (token === "andThen") groups.push([]);
    else groups[groups.length - 1].push(token);
  }
  const segments = [];
  for (const group of groups) {
    if (group.length === 0) return null;
    let upToMs = null;
    let primaryTokens = group;
    const pos = group.indexOf("upTo");
    if (pos !== -1) {
      const rest = group.slice(pos + 1);
      if (rest.length !== 1) return null;
      upToMs = parseDuration(rest[0]);
      if (upToMs === null) return null;
      primaryTokens = group.slice(0, pos);
    }
    if (primaryTokens.length === 0) return null;
    const primary = parsePrimary(primaryTokens.join(" "));
    if (primary === null) return null;
    segments.push({ primary, upToMs });
  }
  const last = segments.length - 1;
  if (segments.some((segment, i) => (i < last) !== (segment.upToMs !== null))) return null;
  return segments;
}

/**
 * Deterministic wait after failed attempt `n` (1-based): walk the segments
 * left to right, handing off to the next segment when the active one's
 * cumulative natural wait would exceed its `upTo` bound. Mirrors
 * crates/keel-core-api's `Schedule::wait_and_jitter` exactly; see
 * conformance/README.md ("Schedule algebra") for the normative spec.
 */
function scheduleWait(segments, attempt) {
  attempt = Math.max(attempt, 1);
  const last = segments.length - 1;
  let i = 0;
  let a = 1;
  let e = 0;
  let emitted = 0;
  for (;;) {
    const { primary, upToMs } = segments[i];
    const wait = primaryWait(primary, a);
    if (i < last && upToMs !== null && e + wait > upToMs) {
      i += 1;
      a = 1;
      e = 0;
      continue;
    }
    emitted += 1;
    if (emitted === attempt) return wait;
    a += 1;
    e += wait;
  }
}

function conditionMatches(cond, cls, httpStatus) {
  if (["conn", "timeout", "cancelled", "other"].includes(cond)) return cond === cls;
  if (cls !== "http" || httpStatus == null) return false;
  if (cond === "4xx") return httpStatus >= 400 && httpStatus <= 499;
  if (cond === "5xx") return httpStatus >= 500 && httpStatus <= 599;
  return /^\d{3}$/.test(cond) && Number(cond) === httpStatus;
}

function invalid(path, msg) {
  return new KeelError("KEEL-E001", `policy invalid at ${path}: ${msg}`);
}

function isTable(v) {
  return v !== null && typeof v === "object" && !Array.isArray(v);
}

/** Reject any key not in `allowed`, mirroring the real core / Rust stub's
 *  `#[serde(deny_unknown_fields)]`: the frozen schema is additionalProperties:
 *  false at every level, and KEEL-E001's "why" includes "an unknown key was
 *  used" — so a typo'd key fails loudly with its path, never silently drops to a
 *  default the user never asked for. */
function rejectUnknownKeys(path, obj, allowed) {
  for (const k of Object.keys(obj))
    if (!allowed.includes(k)) throw invalid(`${path}.${k}`, "unknown key");
}

function validateTargetPolicy(path, v) {
  if (!isTable(v)) throw invalid(path, "expected a table");
  rejectUnknownKeys(path, v, [
    "timeout",
    "retry",
    "breaker",
    "rate",
    "cache",
    "idempotency",
    "fallback",
    "budget",
  ]);
  if (v.timeout !== undefined && parseDuration(v.timeout) === null)
    throw invalid(path, "bad timeout duration");
  if (v.retry !== undefined) {
    if (!isTable(v.retry)) throw invalid(path, "retry must be a table");
    rejectUnknownKeys(`${path}.retry`, v.retry, ["attempts", "schedule", "on"]);
    const { attempts, schedule, on } = v.retry;
    if (attempts !== undefined && (!Number.isInteger(attempts) || attempts < 1))
      throw invalid(path, "retry.attempts must be an integer >= 1");
    if (schedule !== undefined && parseSchedule(schedule) === null)
      throw invalid(path, "unparseable retry.schedule");
    if (on !== undefined) {
      if (!Array.isArray(on)) throw invalid(path, "retry.on must be an array");
      for (const c of on) {
        const known =
          typeof c === "string" &&
          (["conn", "timeout", "cancelled", "other", "4xx", "5xx"].includes(c) ||
            // Frozen schema errorCondition grammar: [1-5][0-9][0-9] (100–599),
            // not any 3-digit string (which accepted 099/999/600).
            /^[1-5][0-9][0-9]$/.test(c));
        if (!known) throw invalid(path, "unknown retry.on condition");
      }
    }
  }
  if (v.breaker !== undefined) {
    if (!isTable(v.breaker)) throw invalid(path, "breaker must be a table");
    rejectUnknownKeys(`${path}.breaker`, v.breaker, [
      "failures",
      "cooldown",
      "window",
      "failure_rate",
      "min_calls",
    ]);
    const { failures, cooldown, window, failure_rate: failureRate, min_calls: minCalls } =
      v.breaker;
    if (failures !== undefined && (!Number.isInteger(failures) || failures < 1))
      throw invalid(path, "breaker.failures must be an integer >= 1");
    if (cooldown !== undefined && parseDuration(cooldown) === null)
      throw invalid(path, "bad breaker.cooldown");
    if (window !== undefined && parseDuration(window) === null)
      throw invalid(path, "bad breaker.window");
    if (
      failureRate !== undefined &&
      (typeof failureRate !== "number" || !(failureRate > 0 && failureRate <= 1))
    )
      throw invalid(path, "breaker.failure_rate must be greater than 0 and at most 1");
    if (minCalls !== undefined && (!Number.isInteger(minCalls) || minCalls < 1))
      throw invalid(path, "breaker.min_calls must be an integer >= 1");
    // Two modes (frozen schema `$defs/breaker`): "Setting `failures` selects
    // count mode." Otherwise a rate-mode knob without BOTH `window` and
    // `failure_rate` is half-configured — reject it rather than silently
    // running count mode on its defaults.
    const hasRatePair = window !== undefined && failureRate !== undefined;
    const hasAnyRateKnob = window !== undefined || failureRate !== undefined || minCalls !== undefined;
    if (failures === undefined && hasAnyRateKnob && !hasRatePair)
      throw invalid(
        path,
        "breaker rate mode requires both `window` and `failure_rate` (count mode sets `failures` instead)"
      );
  }
  if (v.rate !== undefined && (typeof v.rate !== "string" || parseRate(v.rate) === null))
    throw invalid(path, "unparseable rate");
  if (v.cache !== undefined) {
    if (!isTable(v.cache)) throw invalid(path, "cache must be a table");
    rejectUnknownKeys(`${path}.cache`, v.cache, ["ttl", "scope", "mode", "key"]);
    if (v.cache.ttl !== undefined && parseDuration(v.cache.ttl) === null)
      throw invalid(path, "bad cache.ttl");
    // Closed enums (parity with the core's serde enums + the Python stub): a
    // typo like scope="persistant" must fail, not silently fall back to a default.
    if (v.cache.scope !== undefined && v.cache.scope !== "memory" && v.cache.scope !== "persistent")
      throw invalid(path, "cache.scope must be memory|persistent");
    if (v.cache.mode !== undefined && v.cache.mode !== "always" && v.cache.mode !== "dev")
      throw invalid(path, "cache.mode must be always|dev");
    if (v.cache.key !== undefined && v.cache.key !== "args" && v.cache.key !== "url")
      throw invalid(path, "cache.key must be args|url");
  }
  if (v.idempotency !== undefined) {
    if (!isTable(v.idempotency)) throw invalid(path, "idempotency must be a table");
    rejectUnknownKeys(`${path}.idempotency`, v.idempotency, ["header"]);
    if (typeof v.idempotency.header !== "string")
      throw invalid(`${path}.idempotency.header`, "header must be a string");
  }
}

const DEFAULT_MIN_CALLS = 10;

/** The mode a resolved `breaker` table selects, with defaults applied —
 *  mirrors `BreakerPolicy::mode` in crates/keel-core-api. Returns
 *  `{ mode: "count", failures }` or
 *  `{ mode: "rate", windowMs, failureRate, minCalls }`. Configure-time
 *  validation already rejected a half-configured rate mode, so `window`/
 *  `failure_rate` are either both present or both irrelevant here. */
function breakerMode(cfg) {
  const { failures, window, failure_rate: failureRate, min_calls: minCalls } = cfg;
  if (failures === undefined && window !== undefined && failureRate !== undefined) {
    return {
      mode: "rate",
      windowMs: parseDuration(window),
      failureRate,
      minCalls: minCalls ?? DEFAULT_MIN_CALLS,
    };
  }
  return { mode: "count", failures: failures ?? DEFAULT_BREAKER_FAILURES };
}

/** Rate mode: prune outcomes that aged out of the window (strictly: one
 *  exactly `windowMs` old is evicted, per conformance/README.md §4), then
 *  record this one. */
function breakerObserve(b, nowMs, windowMs, failed) {
  while (b.outcomes.length > 0 && nowMs - b.outcomes[0][0] >= windowMs) b.outcomes.shift();
  b.outcomes.push([nowMs, failed]);
}

/** Rate mode's trip condition over the (already-pruned) window. */
function breakerWindowRateReached(b, failureRate, minCalls) {
  const total = b.outcomes.length;
  if (total < minCalls) return false;
  const failed = b.outcomes.filter(([, f]) => f).length;
  return failed / total >= failureRate;
}

/** Token bucket: burst capacity `limit`, continuous refill of `limit` per
 *  `windowMs`. Bit-identical to the real core's `TokenBucket::plan_admit`
 *  (crates/keel-core/src/engine.rs) — same fixed-point integer arithmetic (1
 *  token = `windowMs` scaled units), so no float drift between languages.
 *  Returns the wait to admit this call (0 = immediate); the caller advances
 *  the virtual clock by it, standing in for the real core's actual sleep. */
function tokenBucketAdmit(cell, nowMs, limit, windowMs) {
  const capacity = limit * windowMs;
  if (!cell.primed) {
    cell.primed = true;
    cell.scaledTokens = capacity;
    cell.lastRefillMs = nowMs;
  }
  const elapsed = Math.max(0, nowMs - cell.lastRefillMs);
  cell.lastRefillMs = Math.max(cell.lastRefillMs, nowMs);
  cell.scaledTokens = Math.min(capacity, cell.scaledTokens + elapsed * limit);
  cell.scaledTokens -= windowMs;
  if (cell.scaledTokens >= 0) return 0;
  const deficit = -cell.scaledTokens;
  return Math.ceil(deficit / limit);
}

export class KeelCoreStub {
  #policy = {};
  #nowMs = 0;
  #traceSeq = 0;
  #breakers = new Map(); // target -> {consecutive, openUntil, opens, outcomes}
  #tokenBuckets = new Map(); // target -> {scaledTokens, lastRefillMs, primed}
  #cache = new Map(); // key -> {expires, payload}
  #metrics = new Map(); // target -> counters

  configure(policy) {
    if (!isTable(policy)) throw invalid("$", "policy document must be a table");
    // Top-level keys are the frozen schema's document properties (journal /
    // telemetry / flows are accepted here and inert in the stub, as in the core).
    rejectUnknownKeys("$", policy, ["defaults", "target", "flows", "journal", "telemetry"]);
    if (policy.defaults !== undefined) {
      if (!isTable(policy.defaults)) throw invalid("defaults", "expected a table");
      rejectUnknownKeys("defaults", policy.defaults, ["outbound", "llm"]);
      for (const key of ["outbound", "llm"])
        if (policy.defaults[key] !== undefined)
          validateTargetPolicy(`defaults.${key}`, policy.defaults[key]);
    }
    if (policy.target !== undefined) {
      if (!isTable(policy.target)) throw invalid("target", "expected a table");
      for (const [name, v] of Object.entries(policy.target))
        validateTargetPolicy(`target."${name}"`, v);
    }
    this.#policy = policy;
  }

  #layer(target, key) {
    const t = this.#policy.target;
    if (isTable(t) && isTable(t[target]) && t[target][key] !== undefined)
      return t[target][key];
    const defaults = this.#policy.defaults ?? {};
    if (target.startsWith("llm:") && isTable(defaults.llm) && defaults.llm[key] !== undefined)
      return defaults.llm[key];
    return isTable(defaults.outbound) ? defaults.outbound[key] : undefined;
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
    return b && b.openUntil !== null && this.#nowMs < b.openUntil ? "open" : "closed";
  }

  execute(request, effect) {
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

    const retry = this.#layer(target, "retry");
    const breakerCfg = this.#layer(target, "breaker");
    const rate = this.#layer(target, "rate");
    const cacheCfg = this.#layer(target, "cache");
    const cacheTtl =
      isTable(cacheCfg) && cacheCfg.ttl !== undefined ? parseDuration(cacheCfg.ttl) : null;

    // cache (outermost layer)
    const argsHash = request.args_hash;
    const cacheKey = cacheTtl !== null && argsHash ? `${target}#${argsHash}` : null;
    if (cacheKey && this.#cache.has(cacheKey)) {
      const { expires, payload } = this.#cache.get(cacheKey);
      if (this.#nowMs < expires) {
        m.cache_hits += 1;
        m.successes += 1;
        out.result = "ok";
        out.payload = payload;
        out.from_cache = true;
        out.breaker = this.#breakerState(target);
        return out;
      }
    }

    // rate limiter (token bucket: burst + continuous refill)
    if (typeof rate === "string") {
      const { limit, windowMs } = parseRate(rate);
      if (!this.#tokenBuckets.has(target))
        this.#tokenBuckets.set(target, { scaledTokens: 0, lastRefillMs: 0, primed: false });
      const cell = this.#tokenBuckets.get(target);
      const wait = tokenBucketAdmit(cell, this.#nowMs, limit, windowMs);
      if (wait > 0) {
        out.throttle_wait_ms = wait;
        out.throttled = true;
        this.#nowMs += wait;
        m.throttled += 1;
      }
    }

    // breaker check (observes post-retry call outcomes)
    let halfOpen = false;
    if (isTable(breakerCfg)) {
      if (!this.#breakers.has(target))
        this.#breakers.set(target, { consecutive: 0, openUntil: null, opens: 0, outcomes: [] });
      const b = this.#breakers.get(target);
      if (b.openUntil !== null) {
        if (this.#nowMs < b.openUntil) {
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
      const res = effect(attempt);
      if (res.status === "ok") {
        m.successes += 1;
        if (isTable(breakerCfg)) {
          const b = this.#breakers.get(target);
          // A live success is only reached while closed or half-open;
          // `openUntil` set here means a probe just closed the breaker.
          const closedAProbe = b.openUntil !== null;
          b.consecutive = 0;
          b.openUntil = null;
          if (closedAProbe) {
            // A closing probe resets the window: the pre-open failure
            // history must not instantly re-trip a freshly-recovered target.
            b.outcomes = [];
          } else {
            const resolved = breakerMode(breakerCfg);
            if (resolved.mode === "rate") breakerObserve(b, this.#nowMs, resolved.windowMs, false);
          }
        }
        if (cacheKey && cacheTtl !== null)
          this.#cache.set(cacheKey, { expires: this.#nowMs + cacheTtl, payload: res.payload });
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
        else
          msg = `${op} failed (${detail}); error class is not retryable per policy. ${message}`;
        terminal = { code, class: cls, message: msg.trimEnd() };
        if (httpStatus != null) terminal.http_status = httpStatus;
        if (res.original !== undefined) terminal.original = res.original;
        break;
      }
      let wait = scheduleWait(schedule, attempt);
      if (res.retry_after_ms != null) wait = Math.max(wait, res.retry_after_ms);
      out.waits_ms.push(wait);
      this.#nowMs += wait;
      m.retries += 1;
    }

    // terminal failure
    m.failures += 1;
    if (isTable(breakerCfg)) {
      const cooldown =
        breakerCfg.cooldown !== undefined
          ? parseDuration(breakerCfg.cooldown)
          : DEFAULT_BREAKER_COOLDOWN_MS;
      const b = this.#breakers.get(target);
      let shouldTrip;
      if (halfOpen) {
        shouldTrip = true; // failed probe: re-open for another full cooldown
      } else {
        const resolved = breakerMode(breakerCfg);
        if (resolved.mode === "count") {
          b.consecutive += 1;
          shouldTrip = b.consecutive >= resolved.failures;
        } else {
          breakerObserve(b, this.#nowMs, resolved.windowMs, true);
          shouldTrip = breakerWindowRateReached(b, resolved.failureRate, resolved.minCalls);
        }
      }
      if (shouldTrip) {
        b.openUntil = this.#nowMs + cooldown;
        b.opens += 1;
        b.consecutive = 0;
        b.outcomes = [];
      }
    }
    out.error = terminal;
    out.breaker = this.#breakerState(target);
    return out;
  }

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
    return { v: 1, clock_ms: this.#nowMs, targets };
  }

  advanceClock(ms) {
    this.#nowMs += ms;
  }
}
