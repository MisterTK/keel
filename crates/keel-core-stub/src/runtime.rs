//! The stub runtime: per-target state machines (breaker, rate window, cache,
//! metrics) on a virtual clock, orchestrated by [`KeelCoreStub`]'s
//! [`KeelCore`] implementation. Semantics: `conformance/README.md`.

use std::collections::{BTreeMap, HashMap};

use keel_core_api::{
    AttemptResult, BreakerState, ENVELOPE_VERSION, ErrorClass, ErrorCode, KeelCore, KeelError,
    Outcome, OutcomeError, Request,
};
use serde::Serialize;
use serde_json::Value;

use crate::policy::{BreakerPolicy, Policy, Rate, ResolvedPolicy, RetryPolicy};

/// The stub never sleeps: waits advance this counter and are recorded in the
/// outcome. Conformance scenarios drive it via `advance_clock`.
#[derive(Debug, Clone, Copy, Default)]
struct VirtualClock {
    now_ms: u64,
}

impl VirtualClock {
    fn advance(&mut self, ms: u64) {
        self.now_ms += ms;
    }
}

/// Count-mode circuit breaker (consecutive terminal failures). Observes
/// post-retry call outcomes — layer order puts it outside the retry loop.
#[derive(Debug, Default)]
struct Breaker {
    consecutive: u64,
    open_until: Option<u64>,
    opens: u64,
}

/// What the breaker decided before a call was attempted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Admission {
    /// Normal operation.
    Closed,
    /// Cooldown elapsed: exactly one probe is admitted.
    HalfOpen,
    /// Still open: fail fast, do not invoke the effect.
    Rejected,
}

impl Breaker {
    fn admit(&self, now_ms: u64) -> Admission {
        match self.open_until {
            Some(until) if now_ms < until => Admission::Rejected,
            Some(_) => Admission::HalfOpen,
            None => Admission::Closed,
        }
    }

    fn state_at(&self, now_ms: u64) -> BreakerState {
        match self.admit(now_ms) {
            Admission::Rejected => BreakerState::Open,
            Admission::Closed | Admission::HalfOpen => BreakerState::Closed,
        }
    }

    fn on_success(&mut self) {
        self.consecutive = 0;
        self.open_until = None;
    }

    fn on_terminal_failure(&mut self, now_ms: u64, config: &BreakerPolicy, admission: Admission) {
        let should_trip = if admission == Admission::HalfOpen {
            true // failed probe: re-open for another full cooldown
        } else {
            self.consecutive += 1;
            self.consecutive >= config.failures.get()
        };
        if should_trip {
            self.open_until = Some(now_ms + config.cooldown.0);
            self.opens += 1;
            self.consecutive = 0;
        }
    }
}

/// Fixed-window rate limiter aligned to clock zero (documented stub
/// simplification; the real core may use a token bucket, which is why
/// scenarios assert `throttled`, never the exact wait).
#[derive(Debug, Default)]
struct RateWindow {
    window: u64,
    count: u64,
}

impl RateWindow {
    /// Admits one call, advancing the clock past the window boundary when the
    /// limit is exhausted. Returns the throttle wait applied (0 = immediate).
    fn admit(&mut self, clock: &mut VirtualClock, rate: Rate) -> u64 {
        let current = clock.now_ms / rate.window_ms;
        if self.window != current {
            self.window = current;
            self.count = 0;
        }
        let mut waited = 0;
        if self.count >= rate.limit.get() {
            let next_window_start = (self.window + 1) * rate.window_ms;
            waited = next_window_start - clock.now_ms;
            clock.now_ms = next_window_start;
            self.window = next_window_start / rate.window_ms;
            self.count = 0;
        }
        self.count += 1;
        waited
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    target: String,
    args_hash: String,
}

#[derive(Debug, Clone)]
struct CacheEntry {
    expires_at: u64,
    payload: Value,
}

impl CacheEntry {
    fn is_fresh(&self, now_ms: u64) -> bool {
        now_ms < self.expires_at
    }
}

#[derive(Debug, Default)]
struct TargetMetrics {
    calls: u64,
    attempts: u64,
    retries: u64,
    successes: u64,
    failures: u64,
    cache_hits: u64,
    throttled: u64,
}

/// One target's row in the `keel_report` document. Field names are the
/// frozen report contract; `successes` includes cache hits and `failures`
/// includes breaker rejections.
#[derive(Debug, Serialize)]
struct TargetReport {
    attempts: u64,
    breaker_opens: u64,
    breaker_state: BreakerState,
    cache_hits: u64,
    calls: u64,
    failures: u64,
    retries: u64,
    successes: u64,
    throttled: u64,
}

#[derive(Debug, Serialize)]
struct Report<'a> {
    v: u32,
    clock_ms: u64,
    targets: BTreeMap<&'a str, TargetReport>,
}

/// In-memory fake of the keel-core surface. See the crate docs for the
/// deliberate simplifications relative to the real core.
#[derive(Debug, Default)]
pub struct KeelCoreStub {
    policy: Policy,
    clock: VirtualClock,
    trace_seq: u64,
    breakers: HashMap<String, Breaker>,
    rate_windows: HashMap<String, RateWindow>,
    cache: HashMap<CacheKey, CacheEntry>,
    metrics: BTreeMap<String, TargetMetrics>,
}

/// Why a failed attempt ended the call, per the normative decision order
/// (conformance/README.md §5).
fn terminal_code(
    retryable: bool,
    attempt: u32,
    max_attempts: u32,
    idempotent: bool,
) -> Option<ErrorCode> {
    if !retryable {
        Some(ErrorCode::NonRetryableError)
    } else if attempt == max_attempts {
        Some(ErrorCode::AttemptsExhausted)
    } else if !idempotent {
        Some(ErrorCode::NonIdempotentNotRetried)
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

fn terminal_message(
    code: ErrorCode,
    request: &Request,
    attempt: u32,
    max_attempts: u32,
    class: ErrorClass,
    http_status: Option<u16>,
    message: &str,
) -> String {
    let detail = match http_status {
        Some(status) => format!("{} {status}", class_str(class)),
        None => class_str(class).to_owned(),
    };
    let text = match code {
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
    text.trim_end().to_owned()
}

impl KeelCoreStub {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn metrics_for(&mut self, target: &str) -> &mut TargetMetrics {
        self.metrics.entry(target.to_owned()).or_default()
    }

    fn breaker_state(&self, target: &str) -> BreakerState {
        self.breakers
            .get(target)
            .map_or(BreakerState::Closed, |b| b.state_at(self.clock.now_ms))
    }

    fn next_outcome(&mut self) -> Outcome {
        self.trace_seq += 1;
        Outcome {
            v: ENVELOPE_VERSION,
            result: "error".to_owned(),
            payload: None,
            error: None,
            attempts: 0,
            from_cache: false,
            waits_ms: Vec::new(),
            throttled: false,
            throttle_wait_ms: 0,
            breaker: BreakerState::Closed,
            trace_id: format!("t-{:06}", self.trace_seq),
        }
    }

    fn cache_lookup(&mut self, key: &CacheKey, out: &mut Outcome) -> bool {
        let Some(entry) = self.cache.get(key) else {
            return false;
        };
        if !entry.is_fresh(self.clock.now_ms) {
            return false;
        }
        out.result = String::from("ok");
        out.payload = Some(entry.payload.clone());
        out.from_cache = true;
        let metrics = self.metrics_for(&key.target);
        metrics.cache_hits += 1;
        metrics.successes += 1;
        true
    }

    /// The retry loop. Returns `Ok(payload)` or the terminal error.
    fn run_attempts(
        &mut self,
        request: &Request,
        retry: &RetryPolicy,
        effect: &mut dyn FnMut(u32) -> AttemptResult,
        out: &mut Outcome,
    ) -> Result<Value, OutcomeError> {
        let max_attempts = retry.attempts.get();
        for attempt in 1..=max_attempts {
            out.attempts = attempt;
            self.metrics_for(&request.target).attempts += 1;
            match effect(attempt) {
                AttemptResult::Ok { payload } => return Ok(payload),
                AttemptResult::Error {
                    class,
                    http_status,
                    retry_after_ms,
                    message,
                    original,
                } => {
                    let retryable = retry.is_retryable(class, http_status);
                    if let Some(code) =
                        terminal_code(retryable, attempt, max_attempts, request.idempotent)
                    {
                        return Err(OutcomeError {
                            code,
                            class,
                            http_status,
                            message: terminal_message(
                                code,
                                request,
                                attempt,
                                max_attempts,
                                class,
                                http_status,
                                &message,
                            ),
                            original,
                        });
                    }
                    let mut wait = retry.schedule.wait_ms(attempt);
                    if let Some(server_says) = retry_after_ms {
                        wait = wait.max(server_says);
                    }
                    out.waits_ms.push(wait);
                    self.clock.advance(wait);
                    self.metrics_for(&request.target).retries += 1;
                }
            }
        }
        unreachable!("loop always returns by the final attempt");
    }

    fn execute_inner(
        &mut self,
        request: &Request,
        resolved: &ResolvedPolicy,
        effect: &mut dyn FnMut(u32) -> AttemptResult,
        out: &mut Outcome,
    ) {
        let target = request.target.as_str();

        // cache (outermost layer)
        let cache_key = match (&resolved.cache, &request.args_hash) {
            (Some(cache), Some(hash)) if cache.ttl.is_some() => Some(CacheKey {
                target: target.to_owned(),
                args_hash: hash.clone(),
            }),
            _ => None,
        };
        if let Some(key) = &cache_key
            && self.cache_lookup(key, out)
        {
            out.breaker = self.breaker_state(target);
            return;
        }

        // rate limiter
        if let Some(rate) = resolved.rate {
            let window = self.rate_windows.entry(target.to_owned()).or_default();
            let waited = window.admit(&mut self.clock, rate);
            if waited > 0 {
                out.throttled = true;
                out.throttle_wait_ms = waited;
                self.metrics_for(target).throttled += 1;
            }
        }

        // breaker admission (observes post-retry call outcomes)
        let admission = match &resolved.breaker {
            Some(_) => {
                let breaker = self.breakers.entry(target.to_owned()).or_default();
                breaker.admit(self.clock.now_ms)
            }
            None => Admission::Closed,
        };
        if admission == Admission::Rejected {
            out.error = Some(OutcomeError {
                code: ErrorCode::BreakerOpen,
                class: ErrorClass::Other,
                http_status: None,
                message: format!("breaker OPEN for {target}: failed fast, call not attempted"),
                original: None,
            });
            out.breaker = BreakerState::Open;
            self.metrics_for(target).failures += 1;
            return;
        }

        // retry loop (a missing retry table means one attempt, default matcher)
        let retry = resolved.retry.clone().unwrap_or_else(|| RetryPolicy {
            attempts: core::num::NonZeroU32::MIN,
            ..RetryPolicy::default()
        });
        match self.run_attempts(request, &retry, effect, out) {
            Ok(payload) => {
                self.metrics_for(target).successes += 1;
                if resolved.breaker.is_some()
                    && let Some(breaker) = self.breakers.get_mut(target)
                {
                    breaker.on_success();
                }
                if let (Some(key), Some(cache)) = (cache_key, &resolved.cache)
                    && let Some(ttl) = cache.ttl
                {
                    self.cache.insert(
                        key,
                        CacheEntry {
                            expires_at: self.clock.now_ms + ttl.0,
                            payload: payload.clone(),
                        },
                    );
                }
                out.result = String::from("ok");
                out.payload = Some(payload);
            }
            Err(error) => {
                self.metrics_for(target).failures += 1;
                if let Some(config) = &resolved.breaker
                    && let Some(breaker) = self.breakers.get_mut(target)
                {
                    breaker.on_terminal_failure(self.clock.now_ms, config, admission);
                }
                out.error = Some(error);
            }
        }
        out.breaker = self.breaker_state(target);
    }
}

impl KeelCore for KeelCoreStub {
    fn configure(&mut self, policy_json: &Value) -> Result<(), KeelError> {
        let policy: Policy =
            serde_path_to_error::deserialize(policy_json).map_err(|e| KeelError {
                code: ErrorCode::PolicyInvalid,
                message: format!("policy invalid at {}: {}", e.path(), e.inner()),
            })?;
        self.policy = policy;
        Ok(())
    }

    fn execute(
        &mut self,
        request: &Request,
        effect: &mut dyn FnMut(u32) -> AttemptResult,
    ) -> Outcome {
        self.metrics_for(&request.target).calls += 1;
        let mut out = self.next_outcome();

        if request.v != ENVELOPE_VERSION {
            out.error = Some(OutcomeError {
                code: ErrorCode::EnvelopeVersion,
                class: ErrorClass::Other,
                http_status: None,
                message: format!("unsupported envelope version {}", request.v),
                original: None,
            });
            self.metrics_for(&request.target).failures += 1;
            return out;
        }

        let resolved = self.policy.resolve(&request.target);
        self.execute_inner(request, &resolved, effect, &mut out);
        out
    }

    fn report(&self) -> Value {
        let targets = self
            .metrics
            .iter()
            .map(|(name, m)| {
                let breaker = self.breakers.get(name);
                let row = TargetReport {
                    attempts: m.attempts,
                    breaker_opens: breaker.map_or(0, |b| b.opens),
                    breaker_state: self.breaker_state(name),
                    cache_hits: m.cache_hits,
                    calls: m.calls,
                    failures: m.failures,
                    retries: m.retries,
                    successes: m.successes,
                    throttled: m.throttled,
                };
                (name.as_str(), row)
            })
            .collect();
        serde_json::to_value(Report {
            v: 1,
            clock_ms: self.clock.now_ms,
            targets,
        })
        .expect("report serialization is infallible")
    }

    fn advance_clock(&mut self, ms: u64) {
        self.clock.advance(ms);
    }
}
