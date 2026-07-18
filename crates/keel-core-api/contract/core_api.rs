// keel — core API contract, contracts-v1.
//
// These serde types are the NORMATIVE definition of every envelope that
// crosses the FFI boundary (contracts/core-ffi.h defines the C ABI; the wire
// encoding is MessagePack of exactly these shapes, JSON in diagnostics and
// reports). The `KeelCore` trait is the logical surface both the real core
// (Team A) and `keel-core-stub` implement; language stubs (Python/Node)
// mirror it 1:1 on native dicts/objects.
//
// This file is included verbatim by the `keel-core-api` crate (plain `//`
// comments here because the file body lands mid-crate via include!).
// Do not edit without an approved contract-change request (CCR).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

pub const ENVELOPE_VERSION: u32 = 1;

/// Stable error taxonomy. String forms ("KEEL-E001") appear in envelopes,
/// logs, and `keel explain`; numeric values are frozen in core-ffi.h.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorCode {
    #[serde(rename = "KEEL-E001")]
    PolicyInvalid,
    #[serde(rename = "KEEL-E002")]
    TargetUnknown,
    #[serde(rename = "KEEL-E003")]
    EnvelopeDecode,
    #[serde(rename = "KEEL-E004")]
    EnvelopeVersion,
    /// The policy is valid, but it asks for a capability this build/configuration
    /// cannot provide (v0.1: async effects inside a durable flow; durable flows
    /// with no journal attached).
    #[serde(rename = "KEEL-E005")]
    UnsupportedConfiguration,
    #[serde(rename = "KEEL-E010")]
    AttemptsExhausted,
    #[serde(rename = "KEEL-E011")]
    Timeout,
    #[serde(rename = "KEEL-E012")]
    BreakerOpen,
    #[serde(rename = "KEEL-E013")]
    RateBudgetExceeded,
    #[serde(rename = "KEEL-E014")]
    NonIdempotentNotRetried,
    #[serde(rename = "KEEL-E015")]
    NonRetryableError,
    #[serde(rename = "KEEL-E016")]
    PollDeadlineExceeded,
    #[serde(rename = "KEEL-E020")]
    CacheCodec,
    #[serde(rename = "KEEL-E030")]
    FlowLeaseHeld,
    #[serde(rename = "KEEL-E031")]
    FlowNondeterminism,
    #[serde(rename = "KEEL-E032")]
    FlowDead,
    #[serde(rename = "KEEL-E033")]
    SideEffectsRecorded,
    #[serde(rename = "KEEL-E040")]
    Internal,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::PolicyInvalid => "KEEL-E001",
            ErrorCode::TargetUnknown => "KEEL-E002",
            ErrorCode::EnvelopeDecode => "KEEL-E003",
            ErrorCode::EnvelopeVersion => "KEEL-E004",
            ErrorCode::UnsupportedConfiguration => "KEEL-E005",
            ErrorCode::AttemptsExhausted => "KEEL-E010",
            ErrorCode::Timeout => "KEEL-E011",
            ErrorCode::BreakerOpen => "KEEL-E012",
            ErrorCode::RateBudgetExceeded => "KEEL-E013",
            ErrorCode::NonIdempotentNotRetried => "KEEL-E014",
            ErrorCode::NonRetryableError => "KEEL-E015",
            ErrorCode::PollDeadlineExceeded => "KEEL-E016",
            ErrorCode::CacheCodec => "KEEL-E020",
            ErrorCode::FlowLeaseHeld => "KEEL-E030",
            ErrorCode::FlowNondeterminism => "KEEL-E031",
            ErrorCode::FlowDead => "KEEL-E032",
            ErrorCode::SideEffectsRecorded => "KEEL-E033",
            ErrorCode::Internal => "KEEL-E040",
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Configuration/internal error (e.g. from `configure`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeelError {
    pub code: ErrorCode,
    pub message: String,
}

impl fmt::Display for KeelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for KeelError {}

/// Typed error classes adapters produce. The core never sees language
/// exceptions — adapters classify into these before crossing the boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorClass {
    /// Connection-level failure (refused, reset, DNS).
    Conn,
    /// The attempt timed out.
    Timeout,
    /// HTTP response with an error status (`http_status` is set).
    Http,
    /// The attempt was cancelled by the caller.
    Cancelled,
    /// Anything else (including effect-callback crashes).
    Other,
}

/// One intercepted call, as submitted by a front end to `execute`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    /// Envelope version; must equal ENVELOPE_VERSION.
    pub v: u32,
    /// Resolved target id, e.g. "api.stripe.com", "llm:openai",
    /// "py:pipeline.enrich.geocode". Resolution (globs, METHOD prefixes)
    /// happens in the front end/adapter; the core receives the exact
    /// policy-table key. (Real-core stretch: core-side pattern matching.)
    pub target: String,
    /// Human-readable operation for traces, e.g. "GET api.stripe.com/v1/charges".
    pub op: String,
    /// Front end's safety judgment: true if the call is safe to repeat
    /// (idempotent method, or an idempotency key was injected). When false,
    /// Keel NEVER retries (KEEL-E014) — DX Level 0 hard rule.
    pub idempotent: bool,
    /// Stable hash of call arguments; cache/journal key material.
    /// None disables caching/journaling for the call.
    #[serde(default)]
    pub args_hash: Option<String>,
}

/// Result of ONE attempt, produced by the effect callback.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AttemptResult {
    Ok {
        payload: Value,
    },
    Error {
        class: ErrorClass,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        http_status: Option<u16>,
        /// Server-provided backoff (Retry-After or provider equivalent).
        /// Overrides the schedule: wait = max(schedule_wait, retry_after_ms).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retry_after_ms: Option<u64>,
        #[serde(default)]
        message: String,
        /// Opaque original-error token the front end can use to re-raise the
        /// original exception unchanged (DX invariant 5). Round-tripped, never
        /// inspected by the core.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        original: Option<Value>,
    },
}

/// Terminal error surfaced in an Outcome.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutcomeError {
    pub code: ErrorCode,
    pub class: ErrorClass,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original: Option<Value>,
}

/// Circuit breaker state as observed after the call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BreakerState {
    Closed,
    Open,
    HalfOpen,
}

/// The result of `execute`: what happened after the full layer chain ran.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Outcome {
    pub v: u32,
    /// "ok" | "error"
    pub result: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<OutcomeError>,
    /// Number of effect attempts actually made (0 on cache hit / breaker open).
    pub attempts: u32,
    pub from_cache: bool,
    /// Retry backoff waits, in order. Excludes rate-limit queueing.
    pub waits_ms: Vec<u64>,
    /// True if the call waited on the rate limiter.
    pub throttled: bool,
    pub throttle_wait_ms: u64,
    pub breaker: BreakerState,
    /// Stable per-core trace id ("t-000001"); joins outcomes to spans/journal.
    pub trace_id: String,
}

/// The logical core surface. The real core implements this behind the C ABI
/// in core-ffi.h; keel-core-stub implements it in-memory. Python/Node stubs
/// mirror the same four operations on native values.
pub trait KeelCore {
    /// Apply a policy document (keel.toml as JSON, per policy.schema.json).
    /// Reconfiguration replaces the previous policy atomically.
    fn configure(&mut self, policy_json: &Value) -> Result<(), KeelError>;

    /// Run one intercepted call through the target's layer chain. `effect`
    /// performs a single attempt (1-based attempt number) in the host
    /// language. Must always return an Outcome — policy failures are
    /// outcomes, not panics/exceptions.
    fn execute(
        &mut self,
        request: &Request,
        effect: &mut dyn FnMut(u32) -> AttemptResult,
    ) -> Outcome;

    /// Deterministic metrics/discovery report (JSON): per-target counters
    /// {calls, attempts, retries, successes, failures, cache_hits, throttled,
    /// breaker_opens, breaker_state} plus {"v", "clock_ms"}.
    fn report(&self) -> Value;

    /// Advance the core's clock (milliseconds). The stub runs on a virtual
    /// clock starting at 0 and never sleeps — waits advance the clock and are
    /// recorded. The real core runs on real time and implements this as a
    /// no-op outside its test harness; conformance harnesses use it to model
    /// the passage of time (breaker cooldowns, cache TTLs).
    fn advance_clock(&mut self, _ms: u64) {}
}
