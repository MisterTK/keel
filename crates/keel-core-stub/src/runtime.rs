//! The stub runtime: per-target state machines (breaker, rate window, cache,
//! metrics) on a virtual clock, orchestrated by [`KeelCoreStub`]'s
//! [`KeelCore`] implementation. Semantics: `conformance/README.md`.

use std::collections::{BTreeMap, HashMap, VecDeque};

use keel_core_api::{
    AttemptResult, BreakerState, ENVELOPE_VERSION, ErrorClass, ErrorCode, KeelCore, KeelError,
    Outcome, OutcomeError, Request,
};
use serde::Serialize;
use serde_json::Value;

use keel_core_api::policy::{
    BreakerMode, BreakerPolicy, Policy, Rate, ResolvedPolicy, RetryPolicy,
};

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

/// Circuit breaker in count mode (consecutive terminal failures) or rate mode
/// (failure rate over a sliding window; `BreakerPolicy::mode` selects).
/// Observes post-retry call outcomes — layer order puts it outside the retry
/// loop. Normative semantics and parity with the real core:
/// `conformance/README.md` §4.
#[derive(Debug, Default)]
struct Breaker {
    /// Count mode: consecutive terminal failures.
    consecutive: u64,
    /// Rate mode: post-retry outcomes `(completed_at_ms, failed)` inside the
    /// trailing window, oldest first. Pruned on every observation; cleared
    /// when the breaker opens or a probe closes it.
    outcomes: VecDeque<(u64, bool)>,
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

    fn on_success(&mut self, now_ms: u64, config: &BreakerPolicy) {
        // A live success is only reached while closed or half-open; an open
        // breaker fails fast (never runs the effect). So `open_until.is_some()`
        // here means a probe just closed the breaker.
        let closed_a_probe = self.open_until.is_some();
        self.consecutive = 0;
        self.open_until = None;
        if closed_a_probe {
            // A closing probe resets the window: the pre-open failure history
            // must not instantly re-trip a freshly-recovered target.
            self.outcomes.clear();
            return;
        }
        if let BreakerMode::Rate { window, .. } = config.mode() {
            self.observe(now_ms, window.0, false);
        }
    }

    fn on_terminal_failure(&mut self, now_ms: u64, config: &BreakerPolicy, admission: Admission) {
        let should_trip = if admission == Admission::HalfOpen {
            true // failed probe: re-open for another full cooldown
        } else {
            match config.mode() {
                BreakerMode::Count { failures } => {
                    self.consecutive += 1;
                    self.consecutive >= failures.get()
                }
                BreakerMode::Rate {
                    window,
                    failure_rate,
                    min_calls,
                } => {
                    self.observe(now_ms, window.0, true);
                    self.window_rate_reached(failure_rate, min_calls)
                }
            }
        };
        if should_trip {
            self.open_until = Some(now_ms + config.cooldown.0);
            self.opens += 1;
            self.consecutive = 0;
            self.outcomes.clear();
        }
    }

    /// Rate mode: prune outcomes that aged out of the window (strictly: an
    /// outcome exactly `window_ms` old is evicted, per
    /// `conformance/README.md` §4), then record this one.
    fn observe(&mut self, now_ms: u64, window_ms: u64, failed: bool) {
        while let Some(&(at, _)) = self.outcomes.front() {
            if now_ms.saturating_sub(at) >= window_ms {
                self.outcomes.pop_front();
            } else {
                break;
            }
        }
        self.outcomes.push_back((now_ms, failed));
    }

    /// Rate mode's trip condition over the (already-pruned) window.
    fn window_rate_reached(&self, failure_rate: f64, min_calls: core::num::NonZeroU32) -> bool {
        let total = self.outcomes.len();
        if (total as u64) < u64::from(min_calls.get()) {
            return false;
        }
        let failed = self.outcomes.iter().filter(|&&(_, f)| f).count();
        #[expect(
            clippy::cast_precision_loss,
            reason = "window counts are bounded by the calls observed within one \
                      breaker window — far below f64's 2^53 exact-integer range"
        )]
        let rate = failed as f64 / total as f64;
        rate >= failure_rate
    }
}

/// Token-bucket rate limiter over the virtual clock: burst capacity is the
/// rate's `limit`, refill is continuous at `limit` per `window`. Exceeding the
/// rate delays the call (`throttled`), never fails it. Bit-identical to the
/// real core's `crates/keel-core/src/engine.rs` `TokenBucket` (parity rule) —
/// same fixed-point integer arithmetic, no float drift.
#[derive(Debug, Default)]
struct TokenBucket {
    /// Tokens in scaled units (1 token = `window_ms` units). Negative means
    /// admissions were already booked ahead of refill.
    scaled_tokens: i128,
    /// Virtual-clock ms of the last refill.
    last_refill_ms: u64,
    /// Whether the bucket has been filled to burst on first use.
    primed: bool,
}

impl TokenBucket {
    /// Plans one admission at `elapsed_ms`, pre-booking the token the call
    /// will consume, and advances the virtual clock by the resulting wait (the
    /// stub's stand-in for the real core physically sleeping it). Returns the
    /// wait applied (0 = immediate).
    fn admit(&mut self, clock: &mut VirtualClock, rate: Rate) -> u64 {
        let elapsed_ms = clock.now_ms;
        let limit = i128::from(rate.limit.get());
        let window = i128::from(rate.window_ms);
        let capacity = limit * window; // burst = `limit` whole tokens
        if !self.primed {
            self.primed = true;
            self.scaled_tokens = capacity;
            self.last_refill_ms = elapsed_ms;
        }
        let elapsed = i128::from(elapsed_ms.saturating_sub(self.last_refill_ms));
        self.last_refill_ms = self.last_refill_ms.max(elapsed_ms);
        self.scaled_tokens = capacity.min(
            self.scaled_tokens
                .saturating_add(elapsed.saturating_mul(limit)),
        );
        self.scaled_tokens -= window;
        let wait = if self.scaled_tokens >= 0 {
            0
        } else {
            // ceil(deficit / refill-per-ms)
            let deficit = -self.scaled_tokens;
            u64::try_from((deficit + limit - 1) / limit).unwrap_or(u64::MAX)
        };
        if wait > 0 {
            clock.advance(wait);
        }
        wait
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
    rate_buckets: HashMap<String, TokenBucket>,
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

        // rate limiter (token bucket: burst + continuous refill)
        if let Some(rate) = resolved.rate {
            let bucket = self.rate_buckets.entry(target.to_owned()).or_default();
            let waited = bucket.admit(&mut self.clock, rate);
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
                if let Some(config) = &resolved.breaker
                    && let Some(breaker) = self.breakers.get_mut(target)
                {
                    breaker.on_success(self.clock.now_ms, config);
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
