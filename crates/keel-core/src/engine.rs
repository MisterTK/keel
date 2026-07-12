//! The engine: per-target state machines on real (tokio) time, orchestrating
//! the fixed layer chain cache → rate → breaker → timeout → retry.
//! Normative semantics: `conformance/README.md`; envelope types:
//! `contracts/core_api.rs`.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, MutexGuard, RwLock};
use std::time::Duration;

use keel_core_api::policy::{BreakerPolicy, Policy, Rate, ResolvedPolicy, RetryPolicy};
use keel_core_api::{
    AttemptResult, BreakerState, ENVELOPE_VERSION, ErrorClass, ErrorCode, KeelError, Outcome,
    OutcomeError, Request,
};
use serde::Serialize;
use serde_json::Value;
use tokio::time::Instant;

/// Count-mode circuit breaker (consecutive terminal failures). Observes
/// post-retry call outcomes — layer order puts it outside the retry loop.
#[derive(Debug, Default)]
struct Breaker {
    consecutive: u64,
    open_until: Option<Instant>,
    opens: u64,
}

/// What the breaker decided before a call was attempted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Admission {
    Closed,
    /// Cooldown elapsed: exactly one probe is admitted.
    HalfOpen,
    /// Still open: fail fast, do not invoke the effect.
    Rejected,
}

impl Breaker {
    fn admit(&self, now: Instant) -> Admission {
        match self.open_until {
            Some(until) if now < until => Admission::Rejected,
            Some(_) => Admission::HalfOpen,
            None => Admission::Closed,
        }
    }

    fn state_at(&self, now: Instant) -> BreakerState {
        match self.admit(now) {
            Admission::Rejected => BreakerState::Open,
            Admission::Closed | Admission::HalfOpen => BreakerState::Closed,
        }
    }

    fn on_success(&mut self) {
        self.consecutive = 0;
        self.open_until = None;
    }

    fn on_terminal_failure(&mut self, now: Instant, config: &BreakerPolicy, admission: Admission) {
        let should_trip = if admission == Admission::HalfOpen {
            true // failed probe: re-open for another full cooldown
        } else {
            self.consecutive += 1;
            self.consecutive >= config.failures.get()
        };
        if should_trip {
            self.open_until = Some(now + Duration::from_millis(config.cooldown.0));
            self.opens += 1;
            self.consecutive = 0;
        }
    }
}

/// Fixed-window rate limiter over engine-elapsed milliseconds. Exceeding the
/// limit delays the call to the next window (`throttled`), never fails it.
#[derive(Debug, Default)]
struct RateWindow {
    window: u64,
    count: u64,
}

impl RateWindow {
    /// Plans one admission at `elapsed_ms`, pre-booking the slot the call
    /// will occupy after sleeping. Returns the wait (0 = immediate).
    fn plan_admit(&mut self, elapsed_ms: u64, rate: Rate) -> u64 {
        let current = elapsed_ms / rate.window_ms;
        if self.window != current {
            self.window = current;
            self.count = 0;
        }
        let mut wait = 0;
        if self.count >= rate.limit.get() {
            let next_window_start = (self.window + 1) * rate.window_ms;
            wait = next_window_start - elapsed_ms;
            self.window += 1;
            self.count = 0;
        }
        self.count += 1;
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
    expires_at: Instant,
    payload: Value,
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

/// One target's row in the `keel_report` document (frozen report contract;
/// `successes` includes cache hits, `failures` includes breaker rejections).
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

#[derive(Debug, Default)]
struct State {
    trace_seq: u64,
    breakers: HashMap<String, Breaker>,
    rate_windows: HashMap<String, RateWindow>,
    cache: HashMap<CacheKey, CacheEntry>,
    metrics: BTreeMap<String, TargetMetrics>,
}

impl State {
    fn metrics_for(&mut self, target: &str) -> &mut TargetMetrics {
        self.metrics.entry(target.to_owned()).or_default()
    }

    fn breaker_state(&self, target: &str, now: Instant) -> BreakerState {
        self.breakers
            .get(target)
            .map_or(BreakerState::Closed, |b| b.state_at(now))
    }
}

/// The result of one attempt, tagged with whether the policy timeout layer
/// (not the adapter) produced the failure — that origin is what turns a
/// terminal outcome into `KEEL-E011`.
#[derive(Debug)]
struct AttemptOutcome {
    result: AttemptResult,
    timed_out_by_layer: bool,
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
        ErrorCode::Timeout => format!(
            "{} exceeded its policy timeout on attempt {attempt}/{max_attempts}. {message}",
            request.op
        ),
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

/// The Keel kernel, Tier 1 scope. One per process; `&self`-concurrent.
#[derive(Debug)]
pub struct Engine {
    started: Instant,
    policy: RwLock<Policy>,
    state: Mutex<State>,
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

impl Engine {
    #[must_use]
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            policy: RwLock::new(Policy::default()),
            state: Mutex::new(State::default()),
        }
    }

    /// Apply a policy document (keel.toml as JSON, per
    /// contracts/policy.schema.json), replacing the previous one atomically.
    /// Rejections are `KEEL-E001` with the exact offending field path.
    pub fn configure(&self, policy_json: &Value) -> Result<(), KeelError> {
        let policy: Policy =
            serde_path_to_error::deserialize(policy_json).map_err(|e| KeelError {
                code: ErrorCode::PolicyInvalid,
                message: format!("policy invalid at {}: {}", e.path(), e.inner()),
            })?;
        *self.policy.write().expect("policy lock poisoned") = policy;
        Ok(())
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().expect("state lock poisoned")
    }

    fn elapsed_ms(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// Run one intercepted call through the target's layer chain. `effect`
    /// performs a single attempt (1-based attempt numbers). Always returns an
    /// `Outcome` — policy failures are outcomes, not panics.
    pub async fn execute<F>(&self, request: &Request, mut effect: F) -> Outcome
    where
        F: AsyncFnMut(u32) -> AttemptResult,
    {
        let target = request.target.as_str();
        let mut out = self.begin_call(target);

        if request.v != ENVELOPE_VERSION {
            out.error = Some(OutcomeError {
                code: ErrorCode::EnvelopeVersion,
                class: ErrorClass::Other,
                http_status: None,
                message: format!("unsupported envelope version {}", request.v),
                original: None,
            });
            self.state().metrics_for(target).failures += 1;
            return out;
        }

        let resolved = self
            .policy
            .read()
            .expect("policy lock poisoned")
            .resolve(target);

        // cache (outermost layer)
        let cache_key = match (&resolved.cache, &request.args_hash) {
            (Some(cache), Some(hash)) if cache.ttl.is_some() => Some(CacheKey {
                target: target.to_owned(),
                args_hash: hash.clone(),
            }),
            _ => None,
        };
        if let Some(key) = &cache_key
            && self.serve_from_cache(key, &mut out)
        {
            return out;
        }

        // rate limiter (lock never held across the sleep)
        if let Some(rate) = resolved.rate {
            self.throttle(target, rate, &mut out).await;
        }

        // breaker admission (observes post-retry call outcomes)
        let admission = self.admit(target, &resolved, &mut out);
        if admission == Admission::Rejected {
            return out;
        }

        // timeout + retry (innermost layers)
        let retry = resolved.retry.clone().unwrap_or_else(|| RetryPolicy {
            attempts: core::num::NonZeroU32::MIN,
            ..RetryPolicy::default()
        });
        let result = self
            .run_attempts(request, &resolved, &retry, &mut effect, &mut out)
            .await;
        self.settle(target, &resolved, admission, cache_key, result, &mut out);
        out
    }

    /// Registers the call and mints its outcome envelope + trace id.
    fn begin_call(&self, target: &str) -> Outcome {
        let mut state = self.state();
        state.metrics_for(target).calls += 1;
        state.trace_seq += 1;
        Outcome {
            v: ENVELOPE_VERSION,
            result: String::from("error"),
            payload: None,
            error: None,
            attempts: 0,
            from_cache: false,
            waits_ms: Vec::new(),
            throttled: false,
            throttle_wait_ms: 0,
            breaker: BreakerState::Closed,
            trace_id: format!("t-{:06}", state.trace_seq),
        }
    }

    /// Serves a fresh cached payload, if any (attempts stays 0).
    fn serve_from_cache(&self, key: &CacheKey, out: &mut Outcome) -> bool {
        let now = Instant::now();
        let mut state = self.state();
        let Some(entry) = state.cache.get(key) else {
            return false;
        };
        if now >= entry.expires_at {
            return false;
        }
        out.result = String::from("ok");
        out.payload = Some(entry.payload.clone());
        out.from_cache = true;
        let metrics = state.metrics_for(&key.target);
        metrics.cache_hits += 1;
        metrics.successes += 1;
        out.breaker = state.breaker_state(&key.target, now);
        true
    }

    /// Delays the call when the target's rate is exhausted (never fails it).
    async fn throttle(&self, target: &str, rate: Rate, out: &mut Outcome) {
        let wait_ms = {
            let elapsed = self.elapsed_ms();
            let mut state = self.state();
            let window = state.rate_windows.entry(target.to_owned()).or_default();
            window.plan_admit(elapsed, rate)
        };
        if wait_ms > 0 {
            out.throttled = true;
            out.throttle_wait_ms = wait_ms;
            self.state().metrics_for(target).throttled += 1;
            tokio::time::sleep(Duration::from_millis(wait_ms)).await;
        }
    }

    /// Consults the target's breaker; on rejection, fills the fail-fast
    /// KEEL-E012 outcome (the effect is never invoked).
    fn admit(&self, target: &str, resolved: &ResolvedPolicy, out: &mut Outcome) -> Admission {
        if resolved.breaker.is_none() {
            return Admission::Closed;
        }
        let now = Instant::now();
        let mut state = self.state();
        let admission = state
            .breakers
            .entry(target.to_owned())
            .or_default()
            .admit(now);
        if admission == Admission::Rejected {
            out.error = Some(OutcomeError {
                code: ErrorCode::BreakerOpen,
                class: ErrorClass::Other,
                http_status: None,
                message: format!("breaker OPEN for {target}: failed fast, call not attempted"),
                original: None,
            });
            out.breaker = BreakerState::Open;
            state.metrics_for(target).failures += 1;
        }
        admission
    }

    /// Books the call's terminal result: metrics, breaker transition, cache
    /// write, and the outcome's payload/error/breaker fields.
    fn settle(
        &self,
        target: &str,
        resolved: &ResolvedPolicy,
        admission: Admission,
        cache_key: Option<CacheKey>,
        result: Result<Value, OutcomeError>,
        out: &mut Outcome,
    ) {
        let now = Instant::now();
        let mut state = self.state();
        match result {
            Ok(payload) => {
                state.metrics_for(target).successes += 1;
                if resolved.breaker.is_some()
                    && let Some(breaker) = state.breakers.get_mut(target)
                {
                    breaker.on_success();
                }
                if let (Some(key), Some(cache)) = (cache_key, &resolved.cache)
                    && let Some(ttl) = cache.ttl
                {
                    state.cache.insert(
                        key,
                        CacheEntry {
                            expires_at: now + Duration::from_millis(ttl.0),
                            payload: payload.clone(),
                        },
                    );
                }
                out.result = String::from("ok");
                out.payload = Some(payload);
            }
            Err(error) => {
                state.metrics_for(target).failures += 1;
                if let Some(config) = &resolved.breaker
                    && let Some(breaker) = state.breakers.get_mut(target)
                {
                    breaker.on_terminal_failure(now, config, admission);
                }
                out.error = Some(error);
            }
        }
        out.breaker = state.breaker_state(target, now);
    }

    /// The timeout-wrapped retry loop. `Ok(payload)` or the terminal error.
    async fn run_attempts<F>(
        &self,
        request: &Request,
        resolved: &ResolvedPolicy,
        retry: &RetryPolicy,
        effect: &mut F,
        out: &mut Outcome,
    ) -> Result<Value, OutcomeError>
    where
        F: AsyncFnMut(u32) -> AttemptResult,
    {
        let target = request.target.as_str();
        let max_attempts = retry.attempts.get();
        for attempt in 1..=max_attempts {
            out.attempts = attempt;
            self.state().metrics_for(target).attempts += 1;

            let attempt_outcome = match resolved.timeout {
                Some(limit) => {
                    match tokio::time::timeout(Duration::from_millis(limit.0), effect(attempt))
                        .await
                    {
                        Ok(result) => AttemptOutcome {
                            result,
                            timed_out_by_layer: false,
                        },
                        Err(_elapsed) => AttemptOutcome {
                            result: AttemptResult::Error {
                                class: ErrorClass::Timeout,
                                http_status: None,
                                retry_after_ms: None,
                                message: format!("no response within {}ms", limit.0),
                                original: None,
                            },
                            timed_out_by_layer: true,
                        },
                    }
                }
                None => AttemptOutcome {
                    result: effect(attempt).await,
                    timed_out_by_layer: false,
                },
            };

            match attempt_outcome.result {
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
                        // A policy-layer timeout is the more precise diagnosis
                        // than "exhausted"/"non-retryable" — except for the
                        // Level 0 non-idempotent rule, which callers must see.
                        let code = if attempt_outcome.timed_out_by_layer
                            && code != ErrorCode::NonIdempotentNotRetried
                        {
                            ErrorCode::Timeout
                        } else {
                            code
                        };
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
                    if retry.schedule.has_jitter() && wait > 0 {
                        wait = fastrand::u64(wait / 2..=wait);
                    }
                    if let Some(server_says) = retry_after_ms {
                        wait = wait.max(server_says);
                    }
                    out.waits_ms.push(wait);
                    self.state().metrics_for(target).retries += 1;
                    tokio::time::sleep(Duration::from_millis(wait)).await;
                }
            }
        }
        unreachable!("loop always returns by the final attempt");
    }

    /// Deterministic metrics/discovery report (sorted keys, no wall-clock
    /// timestamps): the same shape `keel_report` freezes in core-ffi.h.
    pub fn report(&self) -> Value {
        let now = Instant::now();
        let state = self.state();
        let targets = state
            .metrics
            .iter()
            .map(|(name, m)| {
                let breaker = state.breakers.get(name);
                let row = TargetReport {
                    attempts: m.attempts,
                    breaker_opens: breaker.map_or(0, |b| b.opens),
                    breaker_state: state.breaker_state(name, now),
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
            clock_ms: self.elapsed_ms(),
            targets,
        })
        .expect("report serialization is infallible")
    }
}
