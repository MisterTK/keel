//! The engine: per-target state machines on real (tokio) time, orchestrating
//! the fixed layer chain cache → rate → breaker → timeout → retry.
//! Normative semantics: `conformance/README.md`; envelope types:
//! `contracts/core_api.rs`.

use core::fmt;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::time::Duration;

use keel_core_api::policy::{
    BreakerMode, BreakerPolicy, CacheScope, DurationMs, JournalLocation, NondeterminismResponse,
    Policy, Rate, ResolvedPolicy, RetryPolicy,
};
use keel_core_api::{
    AttemptResult, BreakerState, ENVELOPE_VERSION, ErrorClass, ErrorCode, KeelError, Outcome,
    OutcomeError, Request,
};
use keel_journal::{
    CacheKey as JournalCacheKey, CallObservation, CallResult, Clock, DiscoveryStore, Journal,
    ObservedError,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::time::Instant;
use tracing::{Instrument, debug, warn};

use crate::events::{CacheStore, EventKind, EventSink, TraceRef};
use crate::journal_backend::{self, JournalBackend};

/// Circuit breaker in count mode (consecutive terminal failures) or rate mode
/// (failure rate over a sliding window; `BreakerPolicy::mode` selects).
/// Observes post-retry call outcomes — layer order puts it outside the retry
/// loop. Normative semantics: conformance/README.md §4.
#[derive(Debug, Default)]
struct Breaker {
    /// Count mode: consecutive terminal failures.
    consecutive: u64,
    /// Rate mode: post-retry outcomes `(completed_at, failed)` inside the
    /// trailing window, oldest first. Pruned on every observation; cleared
    /// when the breaker opens or a probe closes it.
    outcomes: VecDeque<(Instant, bool)>,
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

/// A breaker state change worth surfacing as a telemetry event. Pure
/// observability — the value never influences an outcome or the report; it
/// exists only so the caller can emit a `tracing` event off the state lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BreakerTransition {
    /// No edge was crossed by this call.
    None,
    /// The breaker tripped OPEN (threshold reached, or a probe failed).
    Opened,
    /// A successful probe closed a previously-open breaker.
    Closed,
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

    fn on_success(&mut self, now: Instant, config: &BreakerPolicy) -> BreakerTransition {
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
            return BreakerTransition::Closed;
        }
        if let BreakerMode::Rate { window, .. } = config.mode() {
            self.observe(now, window, false);
        }
        BreakerTransition::None
    }

    fn on_terminal_failure(
        &mut self,
        now: Instant,
        config: &BreakerPolicy,
        admission: Admission,
    ) -> BreakerTransition {
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
                    self.observe(now, window, true);
                    self.window_rate_reached(failure_rate, min_calls)
                }
            }
        };
        if should_trip {
            self.open_until = Some(now + Duration::from_millis(config.cooldown.0));
            self.opens += 1;
            self.consecutive = 0;
            self.outcomes.clear();
            BreakerTransition::Opened
        } else {
            BreakerTransition::None
        }
    }

    /// Rate mode: prune outcomes that aged out of the window (strictly: an
    /// outcome exactly `window` old is evicted, per conformance/README.md §4),
    /// then record this one.
    fn observe(&mut self, now: Instant, window: DurationMs, failed: bool) {
        let window = Duration::from_millis(window.0);
        while let Some(&(at, _)) = self.outcomes.front() {
            if now.duration_since(at) >= window {
                self.outcomes.pop_front();
            } else {
                break;
            }
        }
        self.outcomes.push_back((now, failed));
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

/// Token-bucket rate limiter over engine-elapsed milliseconds (dx-spec §4.1
/// promises token-bucket rate limiting), bit-identical to every stub (parity
/// rule — `crates/keel-core-stub`, `python/keel-core-stub`,
/// `node/keel-core-stub`). Burst capacity is the rate's `limit`; refill is
/// continuous at `limit` per `window`. Exceeding the rate delays the call
/// (`throttled`), never fails it.
///
/// All arithmetic is integer fixed-point — token amounts are scaled by
/// `window_ms`, so one token is `window_ms` scaled units and refill is exactly
/// `limit` scaled units per elapsed millisecond. No float drift: identical
/// call timings plan identical waits, so conformance scenarios may assert the
/// exact `throttle_wait_ms` (conformance/README.md §3).
#[derive(Debug, Default)]
struct TokenBucket {
    /// Tokens in scaled units (1 token = `window_ms` units). Negative means
    /// admissions were already booked ahead of refill — queued waiters, each
    /// spaced `window/limit` apart.
    scaled_tokens: i128,
    /// Engine-elapsed ms of the last refill.
    last_refill_ms: u64,
    /// Whether the bucket has been filled to burst on first use (a `Default`
    /// bucket cannot know the rate yet).
    primed: bool,
}

impl TokenBucket {
    /// Plans one admission at `elapsed_ms`, pre-booking the token the call
    /// will consume after sleeping. Returns the wait (0 = immediate): the time
    /// until continuous refill covers this booking's deficit.
    fn plan_admit(&mut self, elapsed_ms: u64, rate: Rate) -> u64 {
        let limit = i128::from(rate.limit.get());
        let window = i128::from(rate.window_ms);
        let capacity = limit * window; // burst = `limit` whole tokens
        if !self.primed {
            self.primed = true;
            self.scaled_tokens = capacity;
            self.last_refill_ms = elapsed_ms;
        }
        // Concurrent planners may observe `elapsed_ms` before taking the state
        // lock, so a reading older than the last refill credits nothing.
        let elapsed = i128::from(elapsed_ms.saturating_sub(self.last_refill_ms));
        self.last_refill_ms = self.last_refill_ms.max(elapsed_ms);
        self.scaled_tokens = capacity.min(
            self.scaled_tokens
                .saturating_add(elapsed.saturating_mul(limit)),
        );
        self.scaled_tokens -= window;
        if self.scaled_tokens >= 0 {
            0
        } else {
            // ceil(deficit / refill-per-ms)
            let deficit = -self.scaled_tokens;
            u64::try_from((deficit + limit - 1) / limit).unwrap_or(u64::MAX)
        }
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

/// Self-describing tag stamped into every persistent cache payload, so a future
/// reader (`keel trace`, a schema migration, a foreign tool) can tell the
/// encoding and its version apart from a bare value — journal.sql specifies the
/// `cache.value` blob as "MessagePack, schema-tagged".
const CACHE_PAYLOAD_SCHEMA: &str = "keel.cache/v1";

/// The schema-tagged envelope the persistent cache stores. Written by reference
/// so the payload is never cloned on the hot write path.
#[derive(Serialize)]
struct CachePayloadRef<'a> {
    schema: &'a str,
    payload: &'a Value,
}

/// The owned form read back from the journal, before its tag is verified.
#[derive(Deserialize)]
struct CachePayloadOwned {
    schema: String,
    payload: Value,
}

/// MessagePack-encode a cache payload with its schema tag.
fn encode_cache_payload(payload: &Value) -> Result<Vec<u8>, rmp_serde::encode::Error> {
    rmp_serde::to_vec_named(&CachePayloadRef {
        schema: CACHE_PAYLOAD_SCHEMA,
        payload,
    })
}

/// Decode a schema-tagged cache payload. A codec failure or an unrecognized tag
/// returns a reason string, so the read path can degrade to a miss rather than
/// surfacing a poisoned entry to the caller.
fn decode_cache_payload(bytes: &[u8]) -> Result<Value, String> {
    let envelope: CachePayloadOwned =
        rmp_serde::from_slice(bytes).map_err(|e| format!("messagepack decode failed: {e}"))?;
    if envelope.schema != CACHE_PAYLOAD_SCHEMA {
        return Err(format!(
            "unrecognized cache payload schema {:?}",
            envelope.schema
        ));
    }
    Ok(envelope.payload)
}

/// Which cache backend serves a call, decided once per `execute`. `Persistent`
/// is chosen only when the policy asks for it *and* a journal is attached;
/// otherwise the in-memory `Memory` path keeps the engine fully functional
/// un-journaled.
#[derive(Debug)]
enum CachePlan {
    None,
    Memory {
        key: CacheKey,
    },
    Persistent {
        key: JournalCacheKey,
        ttl: DurationMs,
    },
}

/// The discovery-recording surface the engine depends on: a single method, so
/// the engine can hold a [`DiscoveryStore`] type-erased regardless of the
/// [`Clock`] it was opened with. Implemented for every `DiscoveryStore<C>`.
pub trait DiscoveryRecorder: Send + Sync {
    /// Fold one observed call into the store; the error is the journal's own.
    fn record(&self, observation: &CallObservation) -> keel_journal::Result<()>;
}

impl<C: Clock> DiscoveryRecorder for DiscoveryStore<C> {
    fn record(&self, observation: &CallObservation) -> keel_journal::Result<()> {
        DiscoveryStore::record(self, observation)
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
    rate_buckets: HashMap<String, TokenBucket>,
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

/// Static label for a breaker state, matching its `snake_case` serialized form,
/// so a span field reads the same as the report/journal.
fn breaker_str(state: BreakerState) -> &'static str {
    match state {
        BreakerState::Closed => "closed",
        BreakerState::Open => "open",
        BreakerState::HalfOpen => "half_open",
    }
}

/// Stamps the terminal outcome onto the `keel.call` span. Every field was
/// declared `Empty` at span open, so on a disabled span each `record` is a
/// no-op and the trivial accessors below cost effectively nothing — the
/// disabled-callsite fast path telemetry must not perturb.
fn record_call_fields(span: &tracing::Span, out: &Outcome) {
    span.record("trace_id", out.trace_id.as_str());
    span.record("result", out.result.as_str());
    if let Some(error) = out.error.as_ref() {
        span.record("error_code", error.code.as_str());
    }
    span.record("attempts", out.attempts);
    span.record("from_cache", out.from_cache);
    span.record("throttled", out.throttled);
    span.record("breaker", breaker_str(out.breaker));
}

/// Emits a breaker state change at debug level (architecture spec §4.5).
/// Called off the state lock; a no-op when nothing changed.
fn emit_breaker_transition(target: &str, transition: BreakerTransition) {
    match transition {
        BreakerTransition::Opened => {
            debug!(target = %target, transition = "opened", "breaker transition");
        }
        BreakerTransition::Closed => {
            debug!(target = %target, transition = "closed", "breaker transition");
        }
        BreakerTransition::None => {}
    }
}

/// Warn (once per offending table) when a breaker sets `failures` alongside
/// rate-mode knobs: the frozen schema's "setting `failures` selects count
/// mode" makes window/failure_rate/min_calls inert, and inert config must be
/// loud (the same rule `configure` applies to `journal`/`telemetry`).
fn warn_inert_breaker_knobs(policy: &Policy) {
    let defaults = &policy.defaults;
    let tables = defaults
        .outbound
        .iter()
        .map(|t| (String::from("defaults.outbound"), t))
        .chain(
            defaults
                .llm
                .iter()
                .map(|t| (String::from("defaults.llm"), t)),
        )
        .chain(
            policy
                .target
                .iter()
                .map(|(name, t)| (format!("target.\"{name}\""), t)),
        );
    for (path, table) in tables {
        if table
            .breaker
            .as_ref()
            .is_some_and(BreakerPolicy::has_inert_rate_knobs)
        {
            warn!(
                "policy {path}.breaker sets `failures` (count mode) alongside rate-mode knobs \
                 (window/failure_rate/min_calls), which are inert in count mode. Remove \
                 `failures` to select rate mode."
            );
        }
    }
}

/// Append the dx-invariant-4 trace reference (`… trace: keel trace <ref>`) to
/// a terminal failure message. `trace` is `Some` only while a live event sink
/// minted a ref for this call, so without a sink — the conformance condition —
/// every implementation stays message-identical (parity rule).
fn with_trace_ref(message: String, trace: Option<&TraceRef>) -> String {
    match trace {
        Some(t) => {
            let sep = if message.ends_with('.') { "" } else { "." };
            format!("{message}{sep} trace: keel trace {t}")
        }
        None => message,
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
///
/// A journal and/or discovery store are optional attachments: the engine is
/// fully functional without either, and neither can change a call's outcome —
/// their I/O failures degrade to a `warn!` (resilience first, honest reporting).
pub struct Engine {
    started: Instant,
    policy: RwLock<Policy>,
    state: Mutex<State>,
    /// Persistence for the `scope = persistent` cache and Tier 2 flows. Behind
    /// a lock because `configure` honors `policy.journal` by (re)attaching the
    /// selected backend; readers clone the `Arc` out, so the lock is never held
    /// across journal I/O (let alone an await).
    journal: RwLock<JournalSlot>,
    /// Traffic ledger fed one observation per `execute`, for `keel init`/`status`.
    discovery: Option<Arc<dyn DiscoveryRecorder>>,
    /// Live NDJSON event feed for `keel tail`/`keel trace` ([`crate::events`]).
    /// `None` (the zero-cost path) unless the environment activates it or a
    /// sink is attached; like journal/discovery it can never change an outcome.
    events: Option<EventSink>,
}

/// The engine's journal attachment plus, when policy selected it, the resolved
/// `file:` path it was opened from — so reapplying an unchanged policy is a
/// no-op instead of a re-open.
#[derive(Default)]
struct JournalSlot {
    journal: Option<Arc<dyn Journal>>,
    /// `Some` only for a policy-selected (`file:`) attachment; construction-time
    /// attachments have no location the engine could compare against.
    policy_path: Option<PathBuf>,
}

impl fmt::Debug for Engine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The trait-object attachments aren't `Debug`; report their presence.
        f.debug_struct("Engine")
            .field("policy", &self.policy)
            .field("state", &self.state)
            .field("journal_attached", &self.current_journal().is_some())
            .field("discovery_attached", &self.discovery.is_some())
            .field("events_attached", &self.events.is_some())
            .finish_non_exhaustive()
    }
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
            journal: RwLock::new(JournalSlot::default()),
            discovery: None,
            // Live events activate from the environment (KEEL_EVENTS / an
            // existing ./.keel dir — see `crate::events`), so every embedding
            // — FFI, PyO3, napi, CLI — inherits the feed with no plumbing.
            events: EventSink::from_env(),
        }
    }

    /// Attach a journal at construction time, enabling the persistent cache
    /// scope. Optional; set at setup, before the engine is shared for
    /// concurrent `execute`. A later [`configure`](Self::configure) whose
    /// policy carries a `journal` key replaces this attachment — the effective
    /// policy is authoritative (spec §4.2).
    pub fn attach_journal(&mut self, journal: impl Journal + 'static) -> &mut Self {
        let slot = self.journal.get_mut().expect("journal lock poisoned");
        slot.journal = Some(Arc::new(journal));
        slot.policy_path = None;
        self
    }

    /// The attached journal, if any — shared (`Arc`) so a [`FlowManager`] can run
    /// its Tier 2 steps over the *same* store the engine caches through. `None`
    /// for an in-memory engine (Tier 2 requires a durable journal). Live: a
    /// `configure` whose policy selects a `journal` location changes what this
    /// returns, so Tier 2 wiring should read it after the engine is configured.
    ///
    /// [`FlowManager`]: crate::FlowManager
    #[must_use]
    pub fn journal(&self) -> Option<Arc<dyn Journal>> {
        self.current_journal()
    }

    /// Clone the current journal out of its slot (the lock never outlives the
    /// statement, so it is never held across journal I/O or an await).
    fn current_journal(&self) -> Option<Arc<dyn Journal>> {
        self.journal
            .read()
            .expect("journal lock poisoned")
            .journal
            .clone()
    }

    /// Attach a discovery store; each `execute` then records one observation.
    /// Optional and failure-isolated — recording never affects an outcome.
    pub fn attach_discovery(&mut self, discovery: impl DiscoveryRecorder + 'static) -> &mut Self {
        self.discovery = Some(Arc::new(discovery));
        self
    }

    /// Attach (or replace) a live event sink. [`Engine::new`] already resolves
    /// one from the environment (see [`crate::events`]); this override exists
    /// for tests and embedders that place the feed elsewhere.
    pub fn attach_events(&mut self, sink: EventSink) -> &mut Self {
        self.events = Some(sink);
        self
    }

    /// The active event sink, if any — its run id anchors this engine's trace
    /// refs, and `flush()` makes the feed current for a same-process reader.
    #[must_use]
    pub fn events(&self) -> Option<&EventSink> {
        self.events.as_ref()
    }

    /// Emit one live event, stamped with engine-elapsed (virtual-clock-safe)
    /// milliseconds. The closure defers construction — and its string clones —
    /// to the active-sink case, so the disabled path costs one branch.
    fn emit_event(&self, kind: impl FnOnce() -> EventKind) {
        if let Some(sink) = self.events.as_ref() {
            sink.emit(self.elapsed_ms(), kind());
        }
    }

    /// Feed event for a consulted cache: hit (the call is served) or miss
    /// (the call proceeds live). Not emitted when no cache plan exists.
    fn emit_cache(&self, out: &Outcome, target: &str, scope: CacheStore, hit: bool) {
        self.emit_event(|| {
            let call = out.trace_id.clone();
            let target = target.to_owned();
            if hit {
                EventKind::CacheHit {
                    call,
                    target,
                    scope,
                }
            } else {
                EventKind::CacheMiss {
                    call,
                    target,
                    scope,
                }
            }
        });
    }

    /// Apply a policy document (keel.toml as JSON, per
    /// contracts/policy.schema.json), replacing the previous one atomically.
    /// Rejections are `KEEL-E001` with the exact offending field path;
    /// a valid policy naming a journal backend this build cannot provide is
    /// `KEEL-E005` (and the previous policy stays in force).
    pub fn configure(&self, policy_json: &Value) -> Result<(), KeelError> {
        let policy: Policy =
            serde_path_to_error::deserialize(policy_json).map_err(|e| KeelError {
                code: ErrorCode::PolicyInvalid,
                message: format!("policy invalid at {}: {}", e.path(), e.inner()),
            })?;
        // `journal` selects the backing store (spec §4.2 — "that override is
        // the entire laptop→enterprise migration"), so it must take effect or
        // fail loudly, never warn-and-ignore. Applied before the policy swap so
        // a rejected location leaves the previous configuration fully in force.
        if let Some(location) = &policy.journal {
            self.apply_journal_location(location)?;
        }
        // `telemetry` is schema-legal and typed + validated, but v0.1 drives
        // OTel export from the environment. Warn loudly rather than silently
        // ignoring a set value, so a user setting an OTLP endpoint is not
        // surprised when it has no effect.
        if policy.telemetry.is_some() {
            warn!(
                "policy `telemetry` is validated but inert in v0.1: OTel export is configured from \
                 the environment (KEEL_OTEL_*). This table has no effect."
            );
        }
        warn_inert_breaker_knobs(&policy);
        *self.policy.write().expect("policy lock poisoned") = policy;
        Ok(())
    }

    /// Honor `policy.journal`: open and attach the backend it names, replacing
    /// any construction-time attachment — the effective policy is
    /// authoritative. (Front ends that want an environment escape hatch such as
    /// `KEEL_JOURNAL` compose it into the effective policy *before* calling
    /// `configure`, per the effective-policy contract.) Reapplying an unchanged
    /// `file:` location is a no-op, so reconfigure loops never re-open the
    /// store or drop its connection state.
    ///
    /// # Errors
    /// - `KEEL-E005` for a backend this build cannot provide (`postgres://`).
    /// - `KEEL-E040` when the selected SQLite file cannot be created/opened.
    fn apply_journal_location(&self, location: &JournalLocation) -> Result<(), KeelError> {
        let backend = JournalBackend::select(location);
        if let JournalBackend::File(path) = &backend {
            let slot = self.journal.read().expect("journal lock poisoned");
            if slot.policy_path.as_deref() == Some(path.as_path()) {
                return Ok(()); // unchanged location: keep the open store
            }
        }
        // Open OFF the lock (filesystem I/O); the brief write below only swaps
        // pointers. Two racing configures both open, last writer wins — the
        // loser's store is just dropped.
        let journal = journal_backend::open(&backend)?;
        let policy_path = match backend {
            JournalBackend::File(path) => {
                debug!(path = %path.display(), "journal selected by policy");
                Some(path)
            }
            JournalBackend::Postgres => None, // unreachable today: open() errors
        };
        let mut slot = self.journal.write().expect("journal lock poisoned");
        slot.journal = Some(journal);
        slot.policy_path = policy_path;
        Ok(())
    }

    /// The configured Tier 2 `flows.on_nondeterminism` response (default
    /// [`NondeterminismResponse::Fail`]), read live so a reconfigure is honored.
    /// The flow manager consults this when a replay `(seq, step_key)` diverges.
    #[must_use]
    pub fn nondeterminism_response(&self) -> NondeterminismResponse {
        self.policy
            .read()
            .expect("policy lock poisoned")
            .flows
            .as_ref()
            .map_or(NondeterminismResponse::default(), |f| f.on_nondeterminism)
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().expect("state lock poisoned")
    }

    fn elapsed_ms(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// Run one intercepted call through the target's layer chain, then record it
    /// for discovery. `effect` performs a single attempt (1-based attempt
    /// numbers). Always returns an `Outcome` — policy failures are outcomes, not
    /// panics, and neither journal nor discovery I/O can change what's returned.
    pub async fn execute<F>(&self, request: &Request, mut effect: F) -> Outcome
    where
        F: AsyncFnMut(u32) -> AttemptResult,
    {
        let started = Instant::now();
        // One span per wrapped call (architecture spec §4.5). Terminal fields
        // are declared `Empty` and recorded from the finished outcome; the
        // per-attempt child spans are opened inside the instrumented chain.
        // `%request.target`/`%request.op` are only formatted when a subscriber
        // is active — the disabled callsite evaluates nothing.
        let span = tracing::info_span!(
            "keel.call",
            target = %request.target,
            op = %request.op,
            trace_id = tracing::field::Empty,
            result = tracing::field::Empty,
            error_code = tracing::field::Empty,
            attempts = tracing::field::Empty,
            from_cache = tracing::field::Empty,
            throttled = tracing::field::Empty,
            breaker = tracing::field::Empty,
        );
        let out = self
            .run_chain(request, &mut effect)
            .instrument(span.clone())
            .await;
        record_call_fields(&span, &out);
        self.observe(request, &out, started);
        // Every call's last feed event, on all paths (cache hit, breaker
        // reject, envelope error, live attempt).
        self.emit_event(|| EventKind::CallEnd {
            call: out.trace_id.clone(),
            target: request.target.clone(),
            result: out.result.clone(),
            code: out.error.as_ref().map(|e| e.code),
            attempts: out.attempts,
        });
        out
    }

    /// The layer chain proper — cache → rate → breaker → timeout → retry —
    /// unchanged in semantics from the journal-free engine. The persistent cache
    /// scope simply swaps the in-memory map for the journal's `cache` table when
    /// a journal is attached; every other layer is byte-for-byte as before.
    async fn run_chain<F>(&self, request: &Request, effect: &mut F) -> Outcome
    where
        F: AsyncFnMut(u32) -> AttemptResult,
    {
        let target = request.target.as_str();
        let mut out = self.begin_call(target);

        // Every call's first feed event; its seq anchors the trace ref that
        // failure messages carry (`None` without a sink — the parity path).
        let trace = self.events.as_ref().map(|sink| {
            let seq = sink.emit(
                self.elapsed_ms(),
                EventKind::CallStart {
                    call: out.trace_id.clone(),
                    target: target.to_owned(),
                    op: request.op.clone(),
                },
            );
            TraceRef {
                run: sink.run_id().to_owned(),
                seq,
            }
        });

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
        let cache_plan = self.plan_cache(target, &resolved, request);
        match &cache_plan {
            CachePlan::Memory { key } => {
                if self.serve_from_cache(key, &mut out) {
                    self.emit_cache(&out, target, CacheStore::Memory, true);
                    return out;
                }
                self.emit_cache(&out, target, CacheStore::Memory, false);
            }
            CachePlan::Persistent { key, .. } => {
                if self.serve_from_persistent(target, key, &mut out) {
                    self.emit_cache(&out, target, CacheStore::Persistent, true);
                    return out;
                }
                self.emit_cache(&out, target, CacheStore::Persistent, false);
            }
            CachePlan::None => {}
        }

        // rate limiter (lock never held across the sleep)
        if let Some(rate) = resolved.rate {
            self.throttle(target, rate, &mut out).await;
        }

        // breaker admission (observes post-retry call outcomes)
        let admission = self.admit(target, &resolved, &mut out, trace.as_ref());
        if admission == Admission::Rejected {
            return out;
        }

        // timeout + retry (innermost layers)
        let retry = resolved.retry.clone().unwrap_or_else(|| RetryPolicy {
            attempts: core::num::NonZeroU32::MIN,
            ..RetryPolicy::default()
        });
        let result = self
            .run_attempts(request, &resolved, &retry, effect, &mut out, trace.as_ref())
            .await;
        // Only the memory scope writes through under the state lock; the
        // persistent scope writes after the lock drops (journal I/O off-lock).
        let memory_key = match &cache_plan {
            CachePlan::Memory { key } => Some(key.clone()),
            _ => None,
        };
        self.settle(target, &resolved, admission, memory_key, result, &mut out);

        if let CachePlan::Persistent { key, ttl } = &cache_plan
            && out.result == "ok"
            && let Some(payload) = &out.payload
        {
            self.write_persistent(target, key, payload, *ttl);
        }
        out
    }

    /// Decide which cache backend (if any) serves this call. Persistent scope
    /// without a journal falls back to the in-memory map — the engine stays
    /// fully functional un-journaled rather than silently dropping caching.
    fn plan_cache(&self, target: &str, resolved: &ResolvedPolicy, request: &Request) -> CachePlan {
        let (Some(cache), Some(hash)) = (resolved.cache.as_ref(), request.args_hash.as_ref())
        else {
            return CachePlan::None;
        };
        let Some(ttl) = cache.ttl else {
            return CachePlan::None;
        };
        match cache.scope {
            CacheScope::Persistent if self.current_journal().is_some() => CachePlan::Persistent {
                key: JournalCacheKey::new(format!("{target}#{hash}")),
                ttl,
            },
            _ => CachePlan::Memory {
                key: CacheKey {
                    target: target.to_owned(),
                    args_hash: hash.clone(),
                },
            },
        }
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

    /// Serves a fresh cached payload, if any (attempts stays 0). An entry found
    /// expired is *removed* here, not just skipped — combined with the sweep on
    /// write ([`settle`](Self::settle)) this bounds the in-memory map to the live
    /// working set rather than every distinct key ever cached.
    fn serve_from_cache(&self, key: &CacheKey, out: &mut Outcome) -> bool {
        let now = Instant::now();
        let mut state = self.state();
        let payload = match state.cache.get(key) {
            Some(entry) if now < entry.expires_at => entry.payload.clone(),
            Some(_) => {
                // Expired: evict so a per-call-varying key set cannot grow the
                // map without bound for the life of the process.
                state.cache.remove(key);
                return false;
            }
            None => return false,
        };
        out.result = String::from("ok");
        out.payload = Some(payload);
        out.from_cache = true;
        let metrics = state.metrics_for(&key.target);
        metrics.cache_hits += 1;
        metrics.successes += 1;
        out.breaker = state.breaker_state(&key.target, now);
        debug!(target = %key.target, scope = "memory", "cache hit");
        true
    }

    /// Serves a fresh persistent cache payload from the journal, if any (attempts
    /// stays 0). The journal owns TTL expiry against its own clock (identical
    /// semantics to the in-memory scope). Any journal or codec failure degrades
    /// to a miss + `warn!`, so the call proceeds to a live attempt — a broken
    /// journal never fails the call. The journal read runs *before* the state
    /// lock is taken, so no lock is held across journal I/O.
    fn serve_from_persistent(
        &self,
        target: &str,
        key: &JournalCacheKey,
        out: &mut Outcome,
    ) -> bool {
        let Some(journal) = self.current_journal() else {
            return false;
        };
        let bytes = match journal.get_cache(key) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => return false,
            Err(error) => {
                warn!(target = %target, error = %error, "persistent cache read failed; serving live");
                return false;
            }
        };
        let payload = match decode_cache_payload(&bytes) {
            Ok(payload) => payload,
            Err(reason) => {
                warn!(target = %target, reason = %reason, "persistent cache entry undecodable; serving live");
                return false;
            }
        };
        let now = Instant::now();
        let mut state = self.state();
        out.result = String::from("ok");
        out.payload = Some(payload);
        out.from_cache = true;
        let metrics = state.metrics_for(target);
        metrics.cache_hits += 1;
        metrics.successes += 1;
        out.breaker = state.breaker_state(target, now);
        debug!(target = %target, scope = "persistent", "cache hit");
        true
    }

    /// Delays the call when the target's rate is exhausted (never fails it).
    async fn throttle(&self, target: &str, rate: Rate, out: &mut Outcome) {
        let wait_ms = {
            let elapsed = self.elapsed_ms();
            let mut state = self.state();
            let bucket = state.rate_buckets.entry(target.to_owned()).or_default();
            bucket.plan_admit(elapsed, rate)
        };
        if wait_ms > 0 {
            out.throttled = true;
            out.throttle_wait_ms = wait_ms;
            self.state().metrics_for(target).throttled += 1;
            // Emitted before the wait so a live tail shows the queueing now.
            self.emit_event(|| EventKind::Throttle {
                call: out.trace_id.clone(),
                target: target.to_owned(),
                wait_ms,
            });
            tokio::time::sleep(Duration::from_millis(wait_ms)).await;
        }
    }

    /// Consults the target's breaker; on rejection, fills the fail-fast
    /// KEEL-E012 outcome (the effect is never invoked).
    fn admit(
        &self,
        target: &str,
        resolved: &ResolvedPolicy,
        out: &mut Outcome,
        trace: Option<&TraceRef>,
    ) -> Admission {
        if resolved.breaker.is_none() {
            return Admission::Closed;
        }
        let now = Instant::now();
        let admission = {
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
                    message: with_trace_ref(
                        format!("breaker OPEN for {target}: failed fast, call not attempted"),
                        trace,
                    ),
                    original: None,
                });
                out.breaker = BreakerState::Open;
                state.metrics_for(target).failures += 1;
            }
            admission
        };
        // Both feed events run off the state lock, like the debug! below.
        if admission == Admission::Rejected {
            self.emit_event(|| EventKind::BreakerReject {
                call: out.trace_id.clone(),
                target: target.to_owned(),
            });
        }
        // Admitting a probe is an OPEN → HALF-OPEN transition (spec §4.5).
        if admission == Admission::HalfOpen {
            debug!(target = %target, transition = "half_open", "breaker transition");
            self.emit_event(|| EventKind::BreakerHalfOpen {
                call: out.trace_id.clone(),
                target: target.to_owned(),
            });
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
        let transition = {
            let mut state = self.state();
            let transition = match result {
                Ok(payload) => {
                    state.metrics_for(target).successes += 1;
                    let mut transition = BreakerTransition::None;
                    if let Some(config) = &resolved.breaker
                        && let Some(breaker) = state.breakers.get_mut(target)
                    {
                        transition = breaker.on_success(now, config);
                    }
                    if let (Some(key), Some(cache)) = (cache_key, &resolved.cache)
                        && let Some(ttl) = cache.ttl
                    {
                        // Sweep expired entries before inserting so the map is
                        // bounded by the live working set, not the total distinct
                        // keys ever seen. O(n) in current entries per cacheable
                        // write — cheap for the small working sets caching targets
                        // in practice, and it keeps a long-lived process from
                        // leaking every payload it ever cached (no LRU/size cap in
                        // v0.1; a `keel fsck`-style bound is future work).
                        state.cache.retain(|_, entry| entry.expires_at > now);
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
                    transition
                }
                Err(error) => {
                    state.metrics_for(target).failures += 1;
                    let mut transition = BreakerTransition::None;
                    if let Some(config) = &resolved.breaker
                        && let Some(breaker) = state.breakers.get_mut(target)
                    {
                        transition = breaker.on_terminal_failure(now, config, admission);
                    }
                    out.error = Some(error);
                    transition
                }
            };
            out.breaker = state.breaker_state(target, now);
            transition
        };
        emit_breaker_transition(target, transition);
        match transition {
            BreakerTransition::Opened => self.emit_event(|| EventKind::BreakerOpen {
                call: out.trace_id.clone(),
                target: target.to_owned(),
                cooldown_ms: resolved.breaker.as_ref().map_or(0, |b| b.cooldown.0),
            }),
            BreakerTransition::Closed => self.emit_event(|| EventKind::BreakerClose {
                call: out.trace_id.clone(),
                target: target.to_owned(),
            }),
            BreakerTransition::None => {}
        }
    }

    /// Writes a live success into the journal's persistent cache (called after
    /// the state lock is dropped, so journal I/O is never under the engine
    /// mutex). Encoding or journal failure degrades to a `warn!`; the outcome the
    /// caller already holds is unaffected.
    fn write_persistent(
        &self,
        target: &str,
        key: &JournalCacheKey,
        payload: &Value,
        ttl: DurationMs,
    ) {
        let Some(journal) = self.current_journal() else {
            return;
        };
        let bytes = match encode_cache_payload(payload) {
            Ok(bytes) => bytes,
            Err(error) => {
                warn!(target = %target, error = %error, "persistent cache encode failed; entry not stored");
                return;
            }
        };
        if let Err(error) = journal.put_cache(key, &bytes, Duration::from_millis(ttl.0)) {
            warn!(target = %target, error = %error, "persistent cache write failed; entry not stored");
        }
    }

    /// Records one observation of a completed call into the discovery store, if
    /// attached. Runs off the state lock, from data already in the `Outcome`.
    /// Failure degrades to a `warn!` — discovery is evidence, never on the
    /// call's critical path.
    fn observe(&self, request: &Request, out: &Outcome, started: Instant) {
        let Some(discovery) = self.discovery.as_ref() else {
            return;
        };
        let latency_ms = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
        let result = if out.from_cache {
            CallResult::CacheHit
        } else if out.result == "ok" {
            CallResult::Success
        } else {
            CallResult::Failure
        };
        let error = out.error.as_ref().map(|e| ObservedError {
            class: e.class,
            http_status: e.http_status,
        });
        let breaker_opened = out
            .error
            .as_ref()
            .is_some_and(|e| e.code == ErrorCode::BreakerOpen);
        let observation = CallObservation {
            target: request.target.clone(),
            result,
            attempts: out.attempts,
            latency_ms,
            throttled: out.throttled,
            breaker_opened,
            error,
        };
        if let Err(error) = discovery.record(&observation) {
            warn!(target = %request.target, error = %error, "discovery record failed; observation dropped");
        }
    }

    /// Runs a single effect invocation through the timeout layer, tagging the
    /// origin of a failure (adapter vs. policy timeout). The effect future is
    /// instrumented with `attempt_span`, so any tracing it emits nests under the
    /// attempt; the timeout branch synthesizes the same `KEEL-E011`-class error
    /// the retry loop diagnoses.
    ///
    /// `timeout` is the effective per-attempt wall-clock deadline, already gated
    /// by the caller: it is `None` for a non-idempotent request (Level 0 hard
    /// rule — never inject a synthetic failure into a call that may already have
    /// committed server-side; the front ends make the same judgment). Note that
    /// on the synchronous bindings (keel-py/keel-ffi/keel-node sync `execute`) a
    /// blocking effect completes within a single poll, so this timer cannot
    /// preempt it — sync callers get their HTTP client's adapter timeout, not the
    /// policy layer's. The policy per-attempt timeout is effective on the async
    /// path (and in-core futures) where the effect actually awaits.
    async fn run_one_attempt<F>(
        &self,
        timeout: Option<DurationMs>,
        effect: &mut F,
        attempt: u32,
        attempt_span: &tracing::Span,
    ) -> AttemptOutcome
    where
        F: AsyncFnMut(u32) -> AttemptResult,
    {
        match timeout {
            Some(limit) => {
                match tokio::time::timeout(
                    Duration::from_millis(limit.0),
                    effect(attempt).instrument(attempt_span.clone()),
                )
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
                result: effect(attempt).instrument(attempt_span.clone()).await,
                timed_out_by_layer: false,
            },
        }
    }

    /// The timeout-wrapped retry loop. `Ok(payload)` or the terminal error
    /// (whose message carries the call's trace ref when a sink is live).
    async fn run_attempts<F>(
        &self,
        request: &Request,
        resolved: &ResolvedPolicy,
        retry: &RetryPolicy,
        effect: &mut F,
        out: &mut Outcome,
        trace: Option<&TraceRef>,
    ) -> Result<Value, OutcomeError>
    where
        F: AsyncFnMut(u32) -> AttemptResult,
    {
        let target = request.target.as_str();
        let max_attempts = retry.attempts.get();
        // Level 0: never arm the per-attempt wall-clock timeout on a
        // non-idempotent request. Firing it would drop the in-flight effect
        // future while the underlying POST may still commit server-side, then
        // hand the caller a synthetic timeout for a call that actually
        // succeeded. The front ends refuse to impose a deadline here for the
        // same reason; the core must not defeat that guard.
        let attempt_timeout = resolved.timeout.filter(|_| request.idempotent);
        for attempt in 1..=max_attempts {
            out.attempts = attempt;
            self.state().metrics_for(target).attempts += 1;
            self.emit_event(|| EventKind::AttemptStart {
                call: out.trace_id.clone(),
                target: target.to_owned(),
                attempt,
            });

            // Child span per attempt (spec §4.5): `class`/`http_status`/`wait_ms`
            // are filled in below once the attempt resolves. The effect future
            // runs inside this span so any adapter tracing nests correctly.
            let attempt_span = tracing::debug_span!(
                "keel.attempt",
                attempt,
                result = tracing::field::Empty,
                class = tracing::field::Empty,
                http_status = tracing::field::Empty,
                wait_ms = tracing::field::Empty,
            );

            let attempt_outcome = self
                .run_one_attempt(attempt_timeout, effect, attempt, &attempt_span)
                .await;

            match attempt_outcome.result {
                AttemptResult::Ok { payload } => {
                    attempt_span.record("result", "ok");
                    return Ok(payload);
                }
                AttemptResult::Error {
                    class,
                    http_status,
                    retry_after_ms,
                    message,
                    original,
                } => {
                    attempt_span.record("result", "error");
                    attempt_span.record("class", class_str(class));
                    if let Some(status) = http_status {
                        attempt_span.record("http_status", status);
                    }
                    self.emit_event(|| EventKind::AttemptError {
                        call: out.trace_id.clone(),
                        target: target.to_owned(),
                        attempt,
                        class,
                        http_status,
                    });
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
                            message: with_trace_ref(
                                terminal_message(
                                    code,
                                    request,
                                    attempt,
                                    max_attempts,
                                    class,
                                    http_status,
                                    &message,
                                ),
                                trace,
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
                    attempt_span.record("wait_ms", wait);
                    out.waits_ms.push(wait);
                    self.state().metrics_for(target).retries += 1;
                    // Emitted before the wait so a live tail shows it now.
                    self.emit_event(|| EventKind::Backoff {
                        call: out.trace_id.clone(),
                        target: target.to_owned(),
                        attempt,
                        wait_ms: wait,
                    });
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

#[cfg(test)]
mod tests {
    use super::{
        Admission, AttemptResult, Breaker, BreakerTransition, ENVELOPE_VERSION, Engine, Instant,
        Request, TokenBucket,
    };
    use core::num::NonZeroU32;
    use core::time::Duration;
    use keel_core_api::policy::{BreakerPolicy, DurationMs, Rate};
    use serde_json::json;

    /// A failure recorded far enough in the past to have aged out of the
    /// window must not count towards a later trip decision: without eviction,
    /// this exact two-failure sequence (11s apart, `window: 10s`) would trip
    /// at `min_calls: 2` (rate 1.0); with eviction the first failure is gone
    /// by the time the second arrives, so the window only ever holds one.
    #[test]
    fn breaker_rate_mode_evicts_outcomes_older_than_the_window() {
        let config = BreakerPolicy {
            failures: None,
            cooldown: DurationMs(15_000),
            window: Some(DurationMs(10_000)),
            failure_rate: Some(0.5),
            min_calls: Some(NonZeroU32::new(2).unwrap()),
        };
        let mut breaker = Breaker::default();
        let t0 = Instant::now();

        assert_eq!(
            breaker.on_terminal_failure(t0, &config, Admission::Closed),
            BreakerTransition::None,
            "one failure is below min_calls"
        );
        let t_11s = t0 + Duration::from_secs(11);
        assert_eq!(
            breaker.on_terminal_failure(t_11s, &config, Admission::Closed),
            BreakerTransition::None,
            "the stale failure must have aged out of the 10s window"
        );
    }

    /// Burst capacity is `limit` tokens, never more — an idle gap refills the
    /// bucket but is clamped at capacity, so it cannot bank unlimited tokens
    /// for a future burst larger than the configured `limit`.
    #[test]
    fn token_bucket_caps_refill_at_burst_capacity() {
        let rate = Rate {
            limit: core::num::NonZeroU64::new(2).unwrap(),
            window_ms: 1000,
        };
        let mut bucket = TokenBucket::default();

        assert_eq!(bucket.plan_admit(0, rate), 0, "burst covers the first call");
        assert_eq!(
            bucket.plan_admit(0, rate),
            0,
            "burst covers the second call"
        );
        assert_eq!(
            bucket.plan_admit(0, rate),
            500,
            "burst drained: paced at window/limit = 500ms"
        );

        // A long idle gap (50s, enough to "earn" 100 tokens at this rate) must
        // not accrue more than the 2-token burst cap.
        assert_eq!(
            bucket.plan_admit(50_000, rate),
            0,
            "capacity refilled to burst"
        );
        assert_eq!(
            bucket.plan_admit(50_000, rate),
            0,
            "both burst tokens available"
        );
        assert_eq!(
            bucket.plan_admit(50_000, rate),
            500,
            "capacity clamp: idle time cannot bank more than `limit` tokens"
        );
    }

    fn req(target: &str, args_hash: &str) -> Request {
        Request {
            v: ENVELOPE_VERSION,
            target: target.to_owned(),
            op: format!("GET {target}"),
            idempotent: true,
            args_hash: Some(args_hash.to_owned()),
        }
    }

    /// The in-memory cache does not grow without bound: an expired entry is
    /// swept when the next cacheable success writes, so a per-call-varying key
    /// set leaves only the live working set behind (finding: unbounded growth).
    #[tokio::test(start_paused = true)]
    async fn in_memory_cache_evicts_expired_entries() {
        let engine = Engine::new();
        engine
            .configure(&json!({
                "target": { "api.catalog.internal": { "cache": { "ttl": "60s" } } }
            }))
            .expect("valid policy");

        engine
            .execute(&req("api.catalog.internal", "k1"), async |_a| {
                AttemptResult::Ok { payload: json!(1) }
            })
            .await;
        assert_eq!(engine.state().cache.len(), 1, "k1 cached");

        // Past k1's 60s TTL: the write for a NEW key sweeps the expired k1.
        tokio::time::advance(Duration::from_secs(61)).await;
        engine
            .execute(&req("api.catalog.internal", "k2"), async |_a| {
                AttemptResult::Ok { payload: json!(2) }
            })
            .await;
        assert_eq!(
            engine.state().cache.len(),
            1,
            "expired k1 evicted on write; only the live k2 remains"
        );

        // A read of the now-swept k1 is a miss (re-runs live), and reading an
        // expired key also evicts it on the read path.
        let out = engine
            .execute(&req("api.catalog.internal", "k1"), async |_a| {
                AttemptResult::Ok { payload: json!(3) }
            })
            .await;
        assert!(!out.from_cache, "expired/evicted key re-runs live");
    }
}
