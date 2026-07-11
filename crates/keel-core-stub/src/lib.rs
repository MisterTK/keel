//! keel-core-stub: an in-memory fake of the keel-core surface.
//!
//! Purpose (sprint plan, Sprint 0): unblock front-end teams before the real
//! core exists. It records calls, applies *trivial but well-defined*
//! resilience semantics, and returns canned outcomes supplied by the caller's
//! effect callback. The exact semantics are specified in
//! `conformance/README.md` and are shared bit-for-bit with the Python and
//! Node stubs; the real core must pass the same conformance scenarios.
//!
//! Simplifications relative to the real core (documented, deliberate):
//! - virtual clock: waits are recorded and advance an internal ms counter,
//!   never slept
//! - jitter is ignored (deterministic waits)
//! - breaker: consecutive-failure count mode only
//! - rate limiter: fixed windows aligned to clock zero
//! - target resolution: exact key match only (no globs), fallback to
//!   `defaults.llm` for `llm:*` targets, then `defaults.outbound`
//! - `timeout` is validated but not enforced (the script injects `timeout`
//!   error classes instead)

use keel_core_api::{
    AttemptResult, BreakerState, ErrorClass, ErrorCode, KeelCore, KeelError, Outcome, OutcomeError,
    Request, ENVELOPE_VERSION,
};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap};

#[derive(Debug, Clone, Copy)]
struct Schedule {
    base_ms: u64,
    factor: f64,
    cap_ms: u64,
}

impl Schedule {
    /// Wait after failed attempt `n` (1-based): min(base * factor^(n-1), cap).
    fn wait(&self, attempt: u32) -> u64 {
        let w = self.base_ms as f64 * self.factor.powi(attempt as i32 - 1);
        w.min(self.cap_ms as f64).round() as u64
    }
}

const DEFAULT_SCHEDULE: Schedule = Schedule {
    base_ms: 200,
    factor: 2.0,
    cap_ms: 30_000,
};
const DEFAULT_ATTEMPTS: u32 = 3;
const DEFAULT_ON: &[&str] = &["conn", "timeout", "429", "5xx"];
const DEFAULT_BREAKER_FAILURES: u64 = 5;
const DEFAULT_BREAKER_COOLDOWN_MS: u64 = 15_000;

fn parse_duration(s: &str) -> Option<u64> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit())?;
    let (num, unit) = s.split_at(split);
    let n: u64 = num.parse().ok()?;
    let mult = match unit {
        "ms" => 1,
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        _ => return None,
    };
    Some(n * mult)
}

/// "N/s" | "N/sec" | "N/min" | "N/h" | "N/hour" -> (limit, window_ms)
fn parse_rate(s: &str) -> Option<(u64, u64)> {
    let (num, unit) = s.trim().split_once('/')?;
    let limit: u64 = num.trim().parse().ok()?;
    if limit == 0 {
        return None;
    }
    let window = match unit.trim() {
        "s" | "sec" => 1_000,
        "min" => 60_000,
        "h" | "hour" => 3_600_000,
        _ => return None,
    };
    Some((limit, window))
}

/// Parses the v0.1 schedule primaries: exp(base, xF[, max D][, jitter]) and
/// fixed(D). Composition (`upTo`/`andThen`) is in the frozen grammar but not
/// implemented by the stub; using it is a configure-time KEEL-E001.
fn parse_schedule(s: &str) -> Option<Schedule> {
    let s = s.trim();
    if let Some(inner) = s.strip_prefix("exp(").and_then(|r| r.strip_suffix(')')) {
        let parts: Vec<&str> = inner.split(',').map(str::trim).collect();
        if parts.len() < 2 {
            return None;
        }
        let base_ms = parse_duration(parts[0])?;
        let factor: f64 = parts[1].strip_prefix('x')?.parse().ok()?;
        let mut cap_ms = u64::MAX;
        for p in &parts[2..] {
            if let Some(d) = p.strip_prefix("max ") {
                cap_ms = parse_duration(d)?;
            } else if *p != "jitter" {
                return None;
            }
        }
        Some(Schedule {
            base_ms,
            factor,
            cap_ms,
        })
    } else if let Some(inner) = s.strip_prefix("fixed(").and_then(|r| r.strip_suffix(')')) {
        let d = parse_duration(inner)?;
        Some(Schedule {
            base_ms: d,
            factor: 1.0,
            cap_ms: d,
        })
    } else {
        None
    }
}

fn class_str(class: ErrorClass) -> &'static str {
    match class {
        ErrorClass::Conn => "conn",
        ErrorClass::Timeout => "timeout",
        ErrorClass::Http => "http",
        ErrorClass::Cancelled => "cancelled",
        ErrorClass::Other => "other",
    }
}

/// Does an error (class + optional HTTP status) match one retry condition?
fn condition_matches(cond: &str, class: ErrorClass, http_status: Option<u16>) -> bool {
    match cond {
        "conn" | "timeout" | "cancelled" | "other" => cond == class_str(class),
        "4xx" => class == ErrorClass::Http && matches!(http_status, Some(400..=499)),
        "5xx" => class == ErrorClass::Http && matches!(http_status, Some(500..=599)),
        exact => {
            class == ErrorClass::Http
                && exact.len() == 3
                && exact.parse::<u16>().ok() == http_status
        }
    }
}

#[derive(Default)]
struct BreakerCell {
    consecutive: u64,
    open_until: Option<u64>,
    opens: u64,
}

#[derive(Default)]
struct TargetMetrics {
    calls: u64,
    attempts: u64,
    retries: u64,
    successes: u64,
    failures: u64,
    cache_hits: u64,
    throttled: u64,
}

/// The per-layer config resolved for one target (see resolution order above).
#[derive(Default, Clone)]
struct Resolved {
    retry: Option<Value>,
    breaker: Option<Value>,
    rate: Option<(u64, u64)>,
    cache_ttl_ms: Option<u64>,
}

#[derive(Default)]
pub struct KeelCoreStub {
    policy: Value,
    now_ms: u64,
    trace_seq: u64,
    breakers: HashMap<String, BreakerCell>,
    rate_windows: HashMap<String, (u64, u64)>, // (window index, count)
    cache: HashMap<String, (u64, Value)>,      // key -> (expires_at, payload)
    metrics: BTreeMap<String, TargetMetrics>,
}

impl KeelCoreStub {
    pub fn new() -> Self {
        Self {
            policy: json!({}),
            ..Default::default()
        }
    }

    fn invalid(path: &str, msg: &str) -> KeelError {
        KeelError {
            code: ErrorCode::PolicyInvalid,
            message: format!("policy invalid at {path}: {msg}"),
        }
    }

    fn validate_target_policy(path: &str, v: &Value) -> Result<(), KeelError> {
        let obj = v
            .as_object()
            .ok_or_else(|| Self::invalid(path, "expected a table"))?;
        if let Some(t) = obj.get("timeout") {
            let s = t
                .as_str()
                .ok_or_else(|| Self::invalid(path, "timeout must be a duration string"))?;
            parse_duration(s).ok_or_else(|| Self::invalid(path, "bad timeout duration"))?;
        }
        if let Some(r) = obj.get("retry") {
            let r = r
                .as_object()
                .ok_or_else(|| Self::invalid(path, "retry must be a table"))?;
            if let Some(a) = r.get("attempts") {
                match a.as_u64() {
                    Some(n) if n >= 1 => {}
                    _ => {
                        return Err(Self::invalid(
                            path,
                            "retry.attempts must be an integer >= 1",
                        ))
                    }
                }
            }
            if let Some(s) = r.get("schedule") {
                let s = s
                    .as_str()
                    .ok_or_else(|| Self::invalid(path, "retry.schedule must be a string"))?;
                parse_schedule(s)
                    .ok_or_else(|| Self::invalid(path, "unparseable retry.schedule"))?;
            }
            if let Some(on) = r.get("on") {
                let on = on
                    .as_array()
                    .ok_or_else(|| Self::invalid(path, "retry.on must be an array"))?;
                for c in on {
                    let c = c
                        .as_str()
                        .ok_or_else(|| Self::invalid(path, "retry.on entries must be strings"))?;
                    let known = matches!(
                        c,
                        "conn" | "timeout" | "cancelled" | "other" | "4xx" | "5xx"
                    ) || (c.len() == 3 && c.parse::<u16>().is_ok());
                    if !known {
                        return Err(Self::invalid(path, "unknown retry.on condition"));
                    }
                }
            }
        }
        if let Some(b) = obj.get("breaker") {
            let b = b
                .as_object()
                .ok_or_else(|| Self::invalid(path, "breaker must be a table"))?;
            if let Some(f) = b.get("failures") {
                match f.as_u64() {
                    Some(n) if n >= 1 => {}
                    _ => {
                        return Err(Self::invalid(
                            path,
                            "breaker.failures must be an integer >= 1",
                        ))
                    }
                }
            }
            if let Some(c) = b.get("cooldown") {
                let s = c.as_str().ok_or_else(|| {
                    Self::invalid(path, "breaker.cooldown must be a duration string")
                })?;
                parse_duration(s).ok_or_else(|| Self::invalid(path, "bad breaker.cooldown"))?;
            }
        }
        if let Some(r) = obj.get("rate") {
            let s = r
                .as_str()
                .ok_or_else(|| Self::invalid(path, "rate must be a string like \"90/s\""))?;
            parse_rate(s).ok_or_else(|| Self::invalid(path, "unparseable rate"))?;
        }
        if let Some(c) = obj.get("cache") {
            let c = c
                .as_object()
                .ok_or_else(|| Self::invalid(path, "cache must be a table"))?;
            if let Some(t) = c.get("ttl") {
                let s = t
                    .as_str()
                    .ok_or_else(|| Self::invalid(path, "cache.ttl must be a duration string"))?;
                parse_duration(s).ok_or_else(|| Self::invalid(path, "bad cache.ttl"))?;
            }
        }
        Ok(())
    }

    /// Per-layer resolution: target entry, else defaults.llm (for llm:*),
    /// else defaults.outbound. A layer set at a more specific level replaces
    /// the whole layer table (no deep merge).
    fn resolve(&self, target: &str) -> Resolved {
        let layer = |key: &str| -> Option<Value> {
            if let Some(v) = self.policy.pointer(&format!("/target/{target}/{key}")) {
                return Some(v.clone());
            }
            if target.starts_with("llm:") {
                if let Some(v) = self.policy.pointer(&format!("/defaults/llm/{key}")) {
                    return Some(v.clone());
                }
            }
            self.policy
                .pointer(&format!("/defaults/outbound/{key}"))
                .cloned()
        };
        Resolved {
            retry: layer("retry"),
            breaker: layer("breaker"),
            rate: layer("rate").and_then(|v| v.as_str().and_then(parse_rate)),
            cache_ttl_ms: layer("cache").and_then(|c| {
                c.get("ttl")
                    .and_then(|t| t.as_str())
                    .and_then(parse_duration)
            }),
        }
    }

    fn met(&mut self, target: &str) -> &mut TargetMetrics {
        self.metrics.entry(target.to_string()).or_default()
    }

    fn breaker_state(&self, target: &str) -> BreakerState {
        match self.breakers.get(target).and_then(|b| b.open_until) {
            Some(until) if self.now_ms < until => BreakerState::Open,
            _ => BreakerState::Closed,
        }
    }

    fn outcome_base(&mut self) -> (String, Outcome) {
        self.trace_seq += 1;
        let trace_id = format!("t-{:06}", self.trace_seq);
        let o = Outcome {
            v: ENVELOPE_VERSION,
            result: "error".into(),
            payload: None,
            error: None,
            attempts: 0,
            from_cache: false,
            waits_ms: vec![],
            throttled: false,
            throttle_wait_ms: 0,
            breaker: BreakerState::Closed,
            trace_id: trace_id.clone(),
        };
        (trace_id, o)
    }
}

impl KeelCore for KeelCoreStub {
    fn configure(&mut self, policy_json: &Value) -> Result<(), KeelError> {
        let obj = policy_json
            .as_object()
            .ok_or_else(|| Self::invalid("$", "policy document must be a table"))?;
        if let Some(defaults) = obj.get("defaults") {
            let d = defaults
                .as_object()
                .ok_or_else(|| Self::invalid("defaults", "expected a table"))?;
            for key in ["outbound", "llm"] {
                if let Some(v) = d.get(key) {
                    Self::validate_target_policy(&format!("defaults.{key}"), v)?;
                }
            }
        }
        if let Some(targets) = obj.get("target") {
            let t = targets
                .as_object()
                .ok_or_else(|| Self::invalid("target", "expected a table"))?;
            for (name, v) in t {
                Self::validate_target_policy(&format!("target.\"{name}\""), v)?;
            }
        }
        self.policy = policy_json.clone();
        Ok(())
    }

    fn execute(
        &mut self,
        request: &Request,
        effect: &mut dyn FnMut(u32) -> AttemptResult,
    ) -> Outcome {
        let target = request.target.clone();
        self.met(&target).calls += 1;
        let pol = self.resolve(&target);
        let (_trace, mut out) = self.outcome_base();

        if request.v != ENVELOPE_VERSION {
            out.error = Some(OutcomeError {
                code: ErrorCode::EnvelopeVersion,
                class: ErrorClass::Other,
                http_status: None,
                message: format!("unsupported envelope version {}", request.v),
                original: None,
            });
            self.met(&target).failures += 1;
            return out;
        }

        // cache (outermost layer)
        let cache_key = match (pol.cache_ttl_ms, &request.args_hash) {
            (Some(_), Some(h)) => Some(format!("{target}#{h}")),
            _ => None,
        };
        if let Some(key) = &cache_key {
            if let Some((expires, payload)) = self.cache.get(key) {
                if self.now_ms < *expires {
                    let payload = payload.clone();
                    let m = self.met(&target);
                    m.cache_hits += 1;
                    m.successes += 1;
                    out.result = "ok".into();
                    out.payload = Some(payload);
                    out.from_cache = true;
                    out.breaker = self.breaker_state(&target);
                    return out;
                }
            }
        }

        // rate limiter (fixed windows on the virtual clock)
        if let Some((limit, window_ms)) = pol.rate {
            let w = self.now_ms / window_ms;
            let cell = self.rate_windows.entry(target.clone()).or_insert((w, 0));
            if cell.0 != w {
                *cell = (w, 0);
            }
            if cell.1 >= limit {
                let next = (cell.0 + 1) * window_ms;
                out.throttle_wait_ms = next - self.now_ms;
                out.throttled = true;
                self.now_ms = next;
                *cell = (next / window_ms, 0);
                self.met(&target).throttled += 1;
            }
            self.rate_windows.get_mut(&target).unwrap().1 += 1;
        }

        // breaker check (observes post-retry call outcomes)
        let (breaker_failures, breaker_cooldown) = match &pol.breaker {
            Some(b) => (
                b.get("failures")
                    .and_then(Value::as_u64)
                    .unwrap_or(DEFAULT_BREAKER_FAILURES),
                b.get("cooldown")
                    .and_then(Value::as_str)
                    .and_then(parse_duration)
                    .unwrap_or(DEFAULT_BREAKER_COOLDOWN_MS),
            ),
            None => (0, 0),
        };
        let mut half_open = false;
        if pol.breaker.is_some() {
            let b = self.breakers.entry(target.clone()).or_default();
            match b.open_until {
                Some(until) if self.now_ms < until => {
                    out.error = Some(OutcomeError {
                        code: ErrorCode::BreakerOpen,
                        class: ErrorClass::Other,
                        http_status: None,
                        message: format!(
                            "breaker OPEN for {target}: failed fast, call not attempted"
                        ),
                        original: None,
                    });
                    out.breaker = BreakerState::Open;
                    self.met(&target).failures += 1;
                    return out;
                }
                Some(_) => half_open = true,
                None => {}
            }
        }

        // retry loop
        let retry = pol.retry.as_ref().and_then(Value::as_object);
        let max_attempts = retry
            .and_then(|r| r.get("attempts"))
            .and_then(Value::as_u64)
            .map(|n| n as u32)
            .unwrap_or(if retry.is_some() { DEFAULT_ATTEMPTS } else { 1 });
        let schedule = retry
            .and_then(|r| r.get("schedule"))
            .and_then(Value::as_str)
            .and_then(parse_schedule)
            .unwrap_or(DEFAULT_SCHEDULE);
        let on: Vec<String> = retry
            .and_then(|r| r.get("on"))
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_else(|| DEFAULT_ON.iter().map(|s| s.to_string()).collect());

        let mut terminal: Option<OutcomeError> = None;
        for attempt in 1..=max_attempts {
            out.attempts = attempt;
            self.met(&target).attempts += 1;
            match effect(attempt) {
                AttemptResult::Ok { payload } => {
                    let m = self.met(&target);
                    m.successes += 1;
                    if pol.breaker.is_some() {
                        let b = self.breakers.entry(target.clone()).or_default();
                        b.consecutive = 0;
                        b.open_until = None;
                    }
                    if let (Some(key), Some(ttl)) = (&cache_key, pol.cache_ttl_ms) {
                        self.cache
                            .insert(key.clone(), (self.now_ms + ttl, payload.clone()));
                    }
                    out.result = "ok".into();
                    out.payload = Some(payload);
                    out.breaker = self.breaker_state(&target);
                    return out;
                }
                AttemptResult::Error {
                    class,
                    http_status,
                    retry_after_ms,
                    message,
                    original,
                } => {
                    let retryable = on.iter().any(|c| condition_matches(c, class, http_status));
                    let code = if !retryable {
                        Some(ErrorCode::NonRetryableError)
                    } else if attempt == max_attempts {
                        Some(ErrorCode::AttemptsExhausted)
                    } else if !request.idempotent {
                        Some(ErrorCode::NonIdempotentNotRetried)
                    } else {
                        None
                    };
                    if let Some(code) = code {
                        let detail = match http_status {
                            Some(s) => format!("{} {s}", class_str(class)),
                            None => class_str(class).to_string(),
                        };
                        let msg = match code {
                            ErrorCode::AttemptsExhausted => format!(
                                "{} failed {attempt}/{max_attempts} attempts (last: {detail}). {message}",
                                request.op
                            ),
                            ErrorCode::NonIdempotentNotRetried => format!(
                                "{} failed ({detail}). Not retried: call is not idempotent — observed, not retried. {message}",
                                request.op
                            ),
                            _ => format!(
                                "{} failed ({detail}); error class is not retryable per policy. {message}",
                                request.op
                            ),
                        };
                        terminal = Some(OutcomeError {
                            code,
                            class,
                            http_status,
                            message: msg.trim_end().to_string(),
                            original,
                        });
                        break;
                    }
                    let mut wait = schedule.wait(attempt);
                    if let Some(ra) = retry_after_ms {
                        wait = wait.max(ra);
                    }
                    out.waits_ms.push(wait);
                    self.now_ms += wait;
                    self.met(&target).retries += 1;
                }
            }
        }

        // terminal failure
        self.met(&target).failures += 1;
        if pol.breaker.is_some() {
            let now = self.now_ms;
            let b = self.breakers.entry(target.clone()).or_default();
            if half_open {
                b.open_until = Some(now + breaker_cooldown);
                b.opens += 1;
                b.consecutive = 0;
            } else {
                b.consecutive += 1;
                if b.consecutive >= breaker_failures {
                    b.open_until = Some(now + breaker_cooldown);
                    b.opens += 1;
                    b.consecutive = 0;
                }
            }
        }
        out.error = terminal;
        out.breaker = self.breaker_state(&target);
        out
    }

    fn report(&self) -> Value {
        let mut targets = Map::new();
        for (name, m) in &self.metrics {
            let breaker = self.breakers.get(name);
            targets.insert(
                name.clone(),
                json!({
                    "attempts": m.attempts,
                    "breaker_opens": breaker.map(|b| b.opens).unwrap_or(0),
                    "breaker_state": match self.breaker_state(name) {
                        BreakerState::Open => "open",
                        _ => "closed",
                    },
                    "cache_hits": m.cache_hits,
                    "calls": m.calls,
                    "failures": m.failures,
                    "retries": m.retries,
                    "successes": m.successes,
                    "throttled": m.throttled,
                }),
            );
        }
        json!({ "v": 1, "clock_ms": self.now_ms, "targets": targets })
    }

    fn advance_clock(&mut self, ms: u64) {
        self.now_ms += ms;
    }
}
