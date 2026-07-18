//! Typed policy model: `keel.toml` (in its JSON form) deserialized straight
//! into structs. Validation *is* the type system — `NonZero*` integers make
//! zero counts unrepresentable, newtypes parse duration/rate/schedule
//! literals in their `Deserialize` impls, and retry conditions are a closed
//! enum. A document that deserializes is a valid policy; anything else is
//! `KEEL-E001` with a precise field path (via `serde_path_to_error`).

use core::fmt;
use core::num::{NonZeroU32, NonZeroU64};
use core::str::FromStr;
use std::collections::BTreeMap;

use crate::ErrorClass;
use serde::Deserialize;

/// A literal that failed to parse; surfaces through serde as the
/// deserialization error message for the offending field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    what: &'static str,
    input: String,
    /// For literals that are grammatical but invalid (e.g. a schedule
    /// composition whose segments can never all be reached): the reason,
    /// appended so the KEEL-E001 first line says what to fix.
    note: Option<&'static str>,
}

impl ParseError {
    fn new(what: &'static str, input: &str) -> Self {
        Self {
            what,
            input: input.to_owned(),
            note: None,
        }
    }

    fn with_note(what: &'static str, input: &str, note: &'static str) -> Self {
        Self {
            what,
            input: input.to_owned(),
            note: Some(note),
        }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.note {
            None => write!(f, "unparseable {} literal: {:?}", self.what, self.input),
            Some(note) => write!(
                f,
                "invalid {} literal: {:?} — {note}",
                self.what, self.input
            ),
        }
    }
}

impl core::error::Error for ParseError {}

/// A duration literal: `200ms`, `30s`, `5m`, `2h`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(try_from = "String")]
pub struct DurationMs(pub u64);

impl FromStr for DurationMs {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || ParseError::new("duration", s);
        let s = s.trim();
        let unit_at = s.find(|c: char| !c.is_ascii_digit()).ok_or_else(err)?;
        let (number, unit) = s.split_at(unit_at);
        let n: u64 = number.parse().map_err(|_| err())?;
        let mult = match unit {
            "ms" => 1,
            "s" => 1_000,
            "m" => 60_000,
            "h" => 3_600_000,
            _ => return Err(err()),
        };
        Ok(Self(n * mult))
    }
}

impl TryFrom<String> for DurationMs {
    type Error = ParseError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

/// A rate literal: `90/s`, `60/min`, `10/h`. A zero limit is unrepresentable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub struct Rate {
    pub limit: NonZeroU64,
    pub window_ms: u64,
}

impl FromStr for Rate {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || ParseError::new("rate", s);
        let (limit, window) = s.trim().split_once('/').ok_or_else(err)?;
        let limit: NonZeroU64 = limit.trim().parse().map_err(|_| err())?;
        let window_ms = match window.trim() {
            "s" | "sec" => 1_000,
            "min" => 60_000,
            "h" | "hour" => 3_600_000,
            _ => return Err(err()),
        };
        Ok(Self { limit, window_ms })
    }
}

impl TryFrom<String> for Rate {
    type Error = ParseError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

/// A retry schedule per contracts/schedule-grammar.ebnf — the full algebra:
/// one or more `andThen`-separated segments, each an `exp`/`fixed` primary
/// with an optional cumulative-wait bound (`upTo`). Semantics are pinned
/// normatively in conformance/README.md ("Schedule algebra"): `upTo` bounds
/// the segment's cumulative *natural* wait and hands off to the next segment;
/// every segment except the last must be bounded and the last never is (both
/// degenerate shapes are configure-time `KEEL-E001`), so a schedule is always
/// a total mapping attempt → wait.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(try_from = "String")]
pub struct Schedule {
    /// Non-empty; the parser enforces `up_to_ms.is_some()` on every segment
    /// except the last and `None` on the last.
    pub segments: Vec<ScheduleSegment>,
}

/// One `andThen` segment: a primary plus its optional `upTo` bound.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScheduleSegment {
    pub primary: SchedulePrimary,
    /// `upTo` bound on this segment's cumulative natural wait, in ms.
    pub up_to_ms: Option<u64>,
}

/// A schedule primary (`exp` / `fixed`) from the frozen grammar.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SchedulePrimary {
    Exp {
        base_ms: u64,
        factor: f64,
        cap_ms: u64,
        jitter: bool,
    },
    Fixed {
        period_ms: u64,
    },
}

impl SchedulePrimary {
    /// Deterministic natural wait at local attempt `a` (1-based):
    /// `min(base * factor^(a-1), cap)`, before any jitter.
    #[expect(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_possible_wrap,
        reason = "backoff arithmetic: values are small and non-negative by construction"
    )]
    fn wait_ms(self, attempt: u32) -> u64 {
        match self {
            Self::Exp {
                base_ms,
                factor,
                cap_ms,
                ..
            } => {
                let wait = base_ms as f64 * factor.powi(attempt as i32 - 1);
                wait.min(cap_ms as f64).round() as u64
            }
            Self::Fixed { period_ms } => period_ms,
        }
    }

    fn jitter(self) -> bool {
        matches!(self, Self::Exp { jitter: true, .. })
    }
}

impl Default for Schedule {
    /// The contract default: `exp(200ms, x2, max 30s, jitter)`.
    fn default() -> Self {
        Self {
            segments: vec![ScheduleSegment {
                primary: SchedulePrimary::Exp {
                    base_ms: 200,
                    factor: 2.0,
                    cap_ms: 30_000,
                    jitter: true,
                },
                up_to_ms: None,
            }],
        }
    }
}

impl Schedule {
    /// Deterministic wait after failed attempt `n` (1-based), before any
    /// jitter or `Retry-After` override.
    #[must_use]
    pub fn wait_ms(&self, attempt: u32) -> u64 {
        self.wait_and_jitter(attempt).0
    }

    /// `(wait, jitter?)` for retry attempt `n` — a pure function of `n`, per
    /// the normative walk in conformance/README.md ("Schedule algebra"):
    /// segments hand off when the next natural wait would push the segment's
    /// cumulative emitted total past its `upTo` bound (an exact fit stays;
    /// handoffs cascade past segments whose bound is below their first wait),
    /// and each segment restarts at local attempt 1 on entry. The jitter flag
    /// is the emitting segment's — the stubs ignore it (virtual clocks); the
    /// real core samples equal jitter, uniform in `[w/2, w]`.
    ///
    /// # Panics
    /// If `segments` is empty (unrepresentable via the parser).
    #[must_use]
    pub fn wait_and_jitter(&self, attempt: u32) -> (u64, bool) {
        let attempt = attempt.max(1);
        let last = self.segments.len() - 1;
        let (mut i, mut a, mut e) = (0_usize, 1_u32, 0_u64);
        let mut emitted = 0_u32;
        loop {
            let segment = self.segments[i];
            let wait = segment.primary.wait_ms(a);
            // A bound on the final segment is unrepresentable via the parser;
            // `i < last` keeps hand-constructed values total anyway.
            if i < last
                && let Some(bound) = segment.up_to_ms
                && e.saturating_add(wait) > bound
            {
                (i, a, e) = (i + 1, 1, 0);
                continue;
            }
            emitted += 1;
            if emitted == attempt {
                return (wait, segment.primary.jitter());
            }
            a += 1;
            e = e.saturating_add(wait);
        }
    }
}

impl FromStr for SchedulePrimary {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || ParseError::new("schedule", s);
        let s = s.trim();
        if let Some(inner) = s.strip_prefix("exp(").and_then(|r| r.strip_suffix(')')) {
            let parts: Vec<&str> = inner.split(',').map(str::trim).collect();
            let [base, factor, rest @ ..] = parts.as_slice() else {
                return Err(err());
            };
            let base_ms = base.parse::<DurationMs>().map_err(|_| err())?.0;
            let factor: f64 = factor
                .strip_prefix('x')
                .ok_or_else(err)?
                .parse()
                .map_err(|_| err())?;
            let mut cap_ms = u64::MAX;
            let mut jitter = false;
            for part in rest {
                if let Some(d) = part.strip_prefix("max ") {
                    cap_ms = d.parse::<DurationMs>().map_err(|_| err())?.0;
                } else if *part == "jitter" {
                    jitter = true;
                } else {
                    return Err(err());
                }
            }
            Ok(Self::Exp {
                base_ms,
                factor,
                cap_ms,
                jitter,
            })
        } else if let Some(inner) = s.strip_prefix("fixed(").and_then(|r| r.strip_suffix(')')) {
            let period_ms = inner.parse::<DurationMs>().map_err(|_| err())?.0;
            Ok(Self::Fixed { period_ms })
        } else {
            Err(err())
        }
    }
}

impl FromStr for Schedule {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || ParseError::new("schedule", s);
        // The grammar's `ws` makes `upTo` / `andThen` space-separated tokens;
        // tokenizing on whitespace runs and rejoining a primary's tokens with
        // single spaces is lossless for the primary parsers (they trim around
        // commas). Keywords never occur inside `exp(…)`/`fixed(…)` in a valid
        // literal, so no paren tracking is needed — misplaced keywords make
        // the primary unparseable, which is the same KEEL-E001.
        let tokens: Vec<&str> = s.split_whitespace().collect();
        if tokens.is_empty() {
            return Err(err());
        }
        let mut segments = Vec::new();
        for segment_tokens in tokens.split(|t| *t == "andThen") {
            let (primary_tokens, up_to_ms) = match segment_tokens.iter().position(|t| *t == "upTo")
            {
                None => (segment_tokens, None),
                Some(pos) => {
                    // exactly `upTo <duration>`, at the segment's tail
                    let [duration] = &segment_tokens[pos + 1..] else {
                        return Err(err());
                    };
                    let bound = duration.parse::<DurationMs>().map_err(|_| err())?.0;
                    (&segment_tokens[..pos], Some(bound))
                }
            };
            if primary_tokens.is_empty() {
                return Err(err());
            }
            let primary: SchedulePrimary = primary_tokens.join(" ").parse().map_err(|_| err())?;
            segments.push(ScheduleSegment { primary, up_to_ms });
        }
        // Shape rule (normative, conformance/README.md "Schedule algebra"):
        // bounded exactly on the non-final segments, so every segment is
        // reachable and every attempt has a wait.
        let last = segments.len() - 1;
        if segments
            .iter()
            .enumerate()
            .any(|(i, segment)| (i < last) != segment.up_to_ms.is_some())
        {
            return Err(ParseError::with_note(
                "schedule",
                s,
                "`upTo` must bound every segment except the last, and never the last \
                 (an unbounded segment never hands off; a bounded tail would leave \
                 attempts without a wait — cap total retrying with `attempts`)",
            ));
        }
        Ok(Self { segments })
    }
}

impl TryFrom<String> for Schedule {
    type Error = ParseError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

/// One retryable-error condition from `retry.on` (closed set; unknown
/// conditions fail configuration instead of silently never matching).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub enum Condition {
    Conn,
    Timeout,
    Cancelled,
    Other,
    Class4xx,
    Class5xx,
    Status(u16),
}

impl Condition {
    pub fn matches(self, class: ErrorClass, http_status: Option<u16>) -> bool {
        match self {
            Self::Conn => class == ErrorClass::Conn,
            Self::Timeout => class == ErrorClass::Timeout,
            Self::Cancelled => class == ErrorClass::Cancelled,
            Self::Other => class == ErrorClass::Other,
            Self::Class4xx => {
                class == ErrorClass::Http && http_status.is_some_and(|s| (400..=499).contains(&s))
            }
            Self::Class5xx => {
                class == ErrorClass::Http && http_status.is_some_and(|s| (500..=599).contains(&s))
            }
            Self::Status(want) => class == ErrorClass::Http && http_status == Some(want),
        }
    }
}

impl FromStr for Condition {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "conn" => Ok(Self::Conn),
            "timeout" => Ok(Self::Timeout),
            "cancelled" => Ok(Self::Cancelled),
            "other" => Ok(Self::Other),
            "4xx" => Ok(Self::Class4xx),
            "5xx" => Ok(Self::Class5xx),
            // Frozen schema errorCondition grammar is `[1-5][0-9][0-9]` (100–599):
            // require three ASCII digits in range, not any 3-char u16 (which
            // accepted `099`→99 and `999`, outside the contract).
            exact if exact.len() == 3 && exact.bytes().all(|b| b.is_ascii_digit()) => {
                let code: u16 = exact
                    .parse()
                    .map_err(|_| ParseError::new("retry condition", s))?;
                if (100..=599).contains(&code) {
                    Ok(Self::Status(code))
                } else {
                    Err(ParseError::new("retry condition", s))
                }
            }
            _ => Err(ParseError::new("retry condition", s)),
        }
    }
}

impl TryFrom<String> for Condition {
    type Error = ParseError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

/// `retry = { attempts, schedule, on }`. `attempts` is the TOTAL attempt
/// budget (first call included) — zero is unrepresentable by type.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RetryPolicy {
    pub attempts: NonZeroU32,
    pub schedule: Schedule,
    pub on: Vec<Condition>,
}

impl RetryPolicy {
    pub const DEFAULT_ATTEMPTS: NonZeroU32 = NonZeroU32::new(3).unwrap();

    /// The contract default retryable set: `["conn", "timeout", "429", "5xx"]`.
    pub fn default_on() -> Vec<Condition> {
        vec![
            Condition::Conn,
            Condition::Timeout,
            Condition::Status(429),
            Condition::Class5xx,
        ]
    }

    pub fn is_retryable(&self, class: ErrorClass, http_status: Option<u16>) -> bool {
        self.on.iter().any(|c| c.matches(class, http_status))
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            attempts: Self::DEFAULT_ATTEMPTS,
            schedule: Schedule::default(),
            on: Self::default_on(),
        }
    }
}

/// `breaker = { failures, cooldown, window, failure_rate, min_calls }`.
///
/// Two modes per the frozen schema (`$defs/breaker`), enforced identically by
/// the real core and every stub — normative rules in `conformance/README.md`:
/// - **count mode**: selected when `failures` is set (or no rate knob is set;
///   `failures` then defaults to 5) — `failures` consecutive terminal failures
///   open the breaker.
/// - **rate mode**: selected when `failures` is absent and both `window` and
///   `failure_rate` are set — trips when the trailing `window` holds at least
///   `min_calls` outcomes (default 10) with `failed/total >= failure_rate`.
///
/// A rate-mode knob without both `window` and `failure_rate` (and without
/// `failures`) is rejected at deserialize time (KEEL-E001): a half-configured
/// mode must fail loudly, never silently degrade to count-mode defaults.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(try_from = "BreakerPolicyDe")]
pub struct BreakerPolicy {
    /// Count-mode threshold. `None` means "not set by the user" — see
    /// [`BreakerPolicy::mode`] for how that selects the mode.
    pub failures: Option<NonZeroU64>,
    pub cooldown: DurationMs,
    pub window: Option<DurationMs>,
    pub failure_rate: Option<f64>,
    pub min_calls: Option<NonZeroU32>,
}

/// The breaker mode a [`BreakerPolicy`] resolves to, with every default
/// applied — the engine/stubs consume this, never the raw knobs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BreakerMode {
    /// `failures` consecutive terminal failures open the breaker.
    Count { failures: NonZeroU64 },
    /// Failure-rate tripping over a sliding window of post-retry outcomes.
    Rate {
        window: DurationMs,
        failure_rate: f64,
        min_calls: NonZeroU32,
    },
}

impl BreakerPolicy {
    /// Schema default for count mode's `failures`.
    pub const DEFAULT_FAILURES: NonZeroU64 = NonZeroU64::new(5).unwrap();
    /// Schema default for rate mode's `min_calls`.
    pub const DEFAULT_MIN_CALLS: NonZeroU32 = NonZeroU32::new(10).unwrap();

    /// The mode this policy selects (schema: "Setting `failures` selects count
    /// mode"), with defaults applied. Deserialization already rejected
    /// half-configured rate mode, so `window`+`failure_rate` are either both
    /// present or irrelevant here.
    #[must_use]
    pub fn mode(&self) -> BreakerMode {
        match (self.failures, self.window, self.failure_rate) {
            (None, Some(window), Some(failure_rate)) => BreakerMode::Rate {
                window,
                failure_rate,
                min_calls: self.min_calls.unwrap_or(Self::DEFAULT_MIN_CALLS),
            },
            (failures, _, _) => BreakerMode::Count {
                failures: failures.unwrap_or(Self::DEFAULT_FAILURES),
            },
        }
    }

    /// Whether count mode was selected *while* rate-mode knobs are present —
    /// those knobs are inert (the schema's precedence), which callers may want
    /// to surface loudly rather than leave silent.
    #[must_use]
    pub fn has_inert_rate_knobs(&self) -> bool {
        self.failures.is_some()
            && (self.window.is_some() || self.failure_rate.is_some() || self.min_calls.is_some())
    }
}

/// The raw deserialized shape of `breaker`, before mode-completeness
/// validation promotes it to [`BreakerPolicy`].
#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct BreakerPolicyDe {
    failures: Option<NonZeroU64>,
    cooldown: DurationMs,
    window: Option<DurationMs>,
    #[serde(deserialize_with = "de_failure_rate")]
    failure_rate: Option<f64>,
    min_calls: Option<NonZeroU32>,
}

impl Default for BreakerPolicyDe {
    fn default() -> Self {
        Self {
            failures: None,
            cooldown: DurationMs(15_000),
            window: None,
            failure_rate: None,
            min_calls: None,
        }
    }
}

impl TryFrom<BreakerPolicyDe> for BreakerPolicy {
    type Error = String;

    fn try_from(de: BreakerPolicyDe) -> Result<Self, Self::Error> {
        let rate_pair = de.window.is_some() && de.failure_rate.is_some();
        let any_rate_knob =
            de.window.is_some() || de.failure_rate.is_some() || de.min_calls.is_some();
        if de.failures.is_none() && any_rate_knob && !rate_pair {
            return Err(String::from(
                "breaker rate mode requires both `window` and `failure_rate` \
                 (count mode sets `failures` instead)",
            ));
        }
        Ok(Self {
            failures: de.failures,
            cooldown: de.cooldown,
            window: de.window,
            failure_rate: de.failure_rate,
            min_calls: de.min_calls,
        })
    }
}

/// Validate `breaker.failure_rate` against the frozen schema range
/// (`exclusiveMinimum: 0, maximum: 1`) at deserialize time, so an out-of-range or
/// NaN value fails configuration with a precise field path (`KEEL-E001`) instead
/// of being silently accepted — the bare `f64` used to take any value.
fn de_failure_rate<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<f64>::deserialize(deserializer)?;
    if let Some(rate) = value
        && !(rate > 0.0 && rate <= 1.0)
    {
        return Err(serde::de::Error::custom(format!(
            "breaker.failure_rate must be greater than 0 and at most 1 (got {rate})"
        )));
    }
    Ok(value)
}

impl Default for BreakerPolicy {
    fn default() -> Self {
        Self {
            failures: None,
            cooldown: DurationMs(15_000),
            window: None,
            failure_rate: None,
            min_calls: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheScope {
    #[default]
    Memory,
    Persistent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheMode {
    #[default]
    Always,
    /// Caches only when `KEEL_ENV != prod` — the LLM dev-loop cache.
    Dev,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheKeySource {
    #[default]
    Args,
    Url,
}

/// `cache = { ttl, scope, mode, key }`. Caching activates only with a `ttl`.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CachePolicy {
    pub ttl: Option<DurationMs>,
    pub scope: CacheScope,
    pub mode: CacheMode,
    pub key: CacheKeySource,
}

/// `idempotency = { header }` — the header is required by the schema.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdempotencyPolicy {
    pub header: String,
}

/// `until = { field, terminal }` — the poll's terminal predicate (CCR-3).
/// Non-emptiness is enforced at deserialize so an unpollable predicate is
/// KEEL-E001 at configure, never a silent never-terminal loop.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "PollUntilRaw")]
pub struct PollUntil {
    pub field: String,
    pub terminal: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PollUntilRaw {
    field: String,
    terminal: Vec<String>,
}

impl TryFrom<PollUntilRaw> for PollUntil {
    type Error = ParseError;

    fn try_from(raw: PollUntilRaw) -> Result<Self, Self::Error> {
        if raw.field.is_empty() {
            return Err(ParseError::new("poll until.field", "(empty)"));
        }
        if raw.terminal.is_empty() {
            return Err(ParseError::new("poll until.terminal", "(empty array)"));
        }
        Ok(Self {
            field: raw.field,
            terminal: raw.terminal,
        })
    }
}

/// `poll = { interval, deadline, until }` — poll-until-terminal (CCR-3).
/// GET/HEAD at Level 0 only; semantics in conformance/README.md ("Poll").
/// `interval` must be nonzero: on a virtual clock a zero interval never
/// approaches `deadline`, looping forever, so it is rejected at deserialize
/// (KEEL-E001) rather than left to hang at runtime.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "PollPolicyRaw")]
pub struct PollPolicy {
    pub interval: DurationMs,
    pub deadline: DurationMs,
    pub until: PollUntil,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PollPolicyRaw {
    interval: DurationMs,
    deadline: DurationMs,
    until: PollUntil,
}

impl TryFrom<PollPolicyRaw> for PollPolicy {
    type Error = ParseError;

    fn try_from(raw: PollPolicyRaw) -> Result<Self, Self::Error> {
        if raw.interval.0 == 0 {
            return Err(ParseError::with_note(
                "poll.interval",
                "0",
                "must be a nonzero duration",
            ));
        }
        Ok(Self {
            interval: raw.interval,
            deadline: raw.deadline,
            until: raw.until,
        })
    }
}

/// One target's policy table. Every layer is optional; a layer set at a more
/// specific level replaces the whole layer table (no deep merge).
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TargetPolicy {
    pub timeout: Option<DurationMs>,
    pub retry: Option<RetryPolicy>,
    pub breaker: Option<BreakerPolicy>,
    pub rate: Option<Rate>,
    pub cache: Option<CachePolicy>,
    pub idempotency: Option<IdempotencyPolicy>,
    pub poll: Option<PollPolicy>,
    pub fallback: Option<Vec<String>>,
    pub budget: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Defaults {
    pub outbound: Option<TargetPolicy>,
    pub llm: Option<TargetPolicy>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NondeterminismResponse {
    #[default]
    Fail,
    Warn,
    Branch,
}

/// What a concurrent same-identity `keel exec` does while the lease is held
/// by a live process (CCR-4). Default `skip` — the mkdir-mutex pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnBusy {
    #[default]
    Skip,
    Wait,
    Fail,
}

/// Tier 2 flow designation — parsed and carried, enforced by the real core.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FlowsPolicy {
    pub entrypoints: Vec<String>,
    pub on_nondeterminism: NondeterminismResponse,
    pub on_busy: OnBusy,
}

/// A journal location literal (`policy.journal`), validated against the frozen
/// schema pattern `^(file:.+|postgres://.+)$` at parse time so a malformed value
/// fails configuration (KEEL-E001) rather than being silently ignored. The real
/// core honors it at configure time: `file:` attaches a SQLite journal at that
/// path (replacing the construction-time default), and `postgres://` fails
/// loudly with KEEL-E005 until a Postgres backend ships.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub struct JournalLocation(pub String);

impl FromStr for JournalLocation {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let valid = s.strip_prefix("file:").is_some_and(|rest| !rest.is_empty())
            || s.strip_prefix("postgres://")
                .is_some_and(|rest| !rest.is_empty());
        if valid {
            Ok(Self(s.to_owned()))
        } else {
            Err(ParseError::new("journal location", s))
        }
    }
}

impl TryFrom<String> for JournalLocation {
    type Error = ParseError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

/// `[telemetry]` (`otlp_endpoint`, `console`). Parsed and carried; `Engine`
/// exposes `otlp_endpoint` back to native front ends (`telemetry_otlp_endpoint`)
/// which feed it to `keel-core`'s `otel::init_otlp` when built with the `otel`
/// feature — the standard `OTEL_*` environment variables take precedence over
/// this table (see `keel-core`'s otel module for the exact precedence rules).
/// `console` (the local pretty-console-summary switch) is validated and
/// carried but has no consumer yet; `Engine::configure` warns on an explicit
/// `false` so the user is not silently surprised.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TelemetryPolicy {
    pub otlp_endpoint: Option<String>,
    pub console: bool,
}

impl Default for TelemetryPolicy {
    fn default() -> Self {
        // Schema default: console = true.
        Self {
            otlp_endpoint: None,
            console: true,
        }
    }
}

/// The whole `keel.toml` document (contracts/policy.schema.json), typed.
///
/// `deny_unknown_fields` at every object level (here and on the layer structs)
/// makes a typo'd or unknown key a configuration error (KEEL-E001 with the exact
/// path via `serde_path_to_error`), honoring the frozen schema's
/// `additionalProperties: false` and E001's "an unknown key was used" — instead
/// of the previous silent drop that ran the target on defaults the user never
/// asked for.
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Policy {
    pub defaults: Defaults,
    pub target: BTreeMap<String, TargetPolicy>,
    pub flows: Option<FlowsPolicy>,
    /// Journal location (schema-validated), honored by the real core at
    /// configure time (see [`JournalLocation`]).
    pub journal: Option<JournalLocation>,
    /// Telemetry config (schema-validated); `otlp_endpoint` is honored by
    /// native front ends (env still wins), `console` is not yet wired — see
    /// [`TelemetryPolicy`].
    pub telemetry: Option<TelemetryPolicy>,
}

/// The per-layer config resolved for one target: target entry, else
/// `defaults.llm` for `llm:*` targets, else `defaults.outbound`.
#[derive(Debug, Clone, Default)]
pub struct ResolvedPolicy {
    pub timeout: Option<DurationMs>,
    pub retry: Option<RetryPolicy>,
    pub breaker: Option<BreakerPolicy>,
    pub rate: Option<Rate>,
    pub cache: Option<CachePolicy>,
    /// `idempotency = { header }` — the knob adapters consult to *inject* a
    /// minted idempotency key on unsafe-method calls (and to recognize a
    /// caller-supplied one). The core itself never injects; injection lives in
    /// the adapter per contracts/adapter-pack.md ("Idempotency-key injection").
    pub idempotency: Option<IdempotencyPolicy>,
    /// `poll = { interval, deadline, until }` — poll-until-terminal (CCR-3).
    pub poll: Option<PollPolicy>,
}

impl Policy {
    pub fn resolve(&self, target: &str) -> ResolvedPolicy {
        ResolvedPolicy {
            timeout: self.layer(target, |t| t.timeout.as_ref()).copied(),
            retry: self.layer(target, |t| t.retry.as_ref()).cloned(),
            breaker: self.layer(target, |t| t.breaker.as_ref()).cloned(),
            rate: self.layer(target, |t| t.rate.as_ref()).copied(),
            cache: self.layer(target, |t| t.cache.as_ref()).cloned(),
            idempotency: self.layer(target, |t| t.idempotency.as_ref()).cloned(),
            poll: self.layer(target, |t| t.poll.as_ref()).cloned(),
        }
    }

    fn layer<'a, T>(
        &'a self,
        target: &str,
        pick: impl Fn(&'a TargetPolicy) -> Option<&'a T>,
    ) -> Option<&'a T> {
        if let Some(t) = self.target.get(target)
            && let Some(v) = pick(t)
        {
            return Some(v);
        }
        if target.starts_with("llm:")
            && let Some(llm) = self.defaults.llm.as_ref()
            && let Some(v) = pick(llm)
        {
            return Some(v);
        }
        self.defaults.outbound.as_ref().and_then(pick)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn duration_literals() {
        assert_eq!("200ms".parse(), Ok(DurationMs(200)));
        assert_eq!("30s".parse(), Ok(DurationMs(30_000)));
        assert_eq!("5m".parse(), Ok(DurationMs(300_000)));
        assert_eq!("2h".parse(), Ok(DurationMs(7_200_000)));
        assert!("30".parse::<DurationMs>().is_err());
        assert!("30sec".parse::<DurationMs>().is_err());
        assert!("-1s".parse::<DurationMs>().is_err());
    }

    #[test]
    fn rate_literals() {
        let rate: Rate = "90/s".parse().unwrap();
        assert_eq!((rate.limit.get(), rate.window_ms), (90, 1_000));
        let rate: Rate = "60/min".parse().unwrap();
        assert_eq!((rate.limit.get(), rate.window_ms), (60, 60_000));
        assert!("0/s".parse::<Rate>().is_err(), "zero limit unrepresentable");
        assert!("10/day".parse::<Rate>().is_err());
    }

    #[test]
    fn schedule_exp_waits_and_cap() {
        let schedule: Schedule = "exp(1s, x2, max 4s)".parse().unwrap();
        let waits: Vec<u64> = (1..=4).map(|n| schedule.wait_ms(n)).collect();
        assert_eq!(waits, [1_000, 2_000, 4_000, 4_000]);
    }

    #[test]
    fn schedule_fixed_and_rejections() {
        assert_eq!(
            "fixed(1s)".parse::<Schedule>(),
            Ok(Schedule {
                segments: vec![ScheduleSegment {
                    primary: SchedulePrimary::Fixed { period_ms: 1_000 },
                    up_to_ms: None,
                }],
            })
        );
        assert!("linear(1s)".parse::<Schedule>().is_err());
    }

    #[test]
    fn schedule_composition_parses_the_spec_example() {
        // architecture-spec §4.1 / the frozen grammar's own example
        let schedule: Schedule = "exp(1s, x2, max 5m) upTo 10m andThen fixed(1m)"
            .parse()
            .unwrap();
        assert_eq!(
            schedule.segments,
            vec![
                ScheduleSegment {
                    primary: SchedulePrimary::Exp {
                        base_ms: 1_000,
                        factor: 2.0,
                        cap_ms: 300_000,
                        jitter: false,
                    },
                    up_to_ms: Some(600_000),
                },
                ScheduleSegment {
                    primary: SchedulePrimary::Fixed { period_ms: 60_000 },
                    up_to_ms: None,
                },
            ]
        );
        // The grammar's ws is "one or more spaces": extra spacing still parses.
        assert_eq!(
            "exp(1s, x2, max 5m)  upTo  10m  andThen  fixed(1m)".parse::<Schedule>(),
            Ok(schedule)
        );
    }

    #[test]
    fn schedule_composition_hands_off_when_the_bound_would_be_overshot() {
        let schedule: Schedule = "exp(1s, x2) upTo 4s andThen fixed(500ms)".parse().unwrap();
        let waits: Vec<u64> = (1..=5).map(|n| schedule.wait_ms(n)).collect();
        // 1s + 2s = 3s fits; the natural 4s would overshoot the 4s bound.
        assert_eq!(waits, [1_000, 2_000, 500, 500, 500]);
    }

    #[test]
    fn schedule_composition_exact_fit_stays_and_cascade_skips() {
        let schedule: Schedule =
            "fixed(1s) upTo 3s andThen fixed(10s) upTo 5s andThen fixed(250ms)"
                .parse()
                .unwrap();
        let waits: Vec<u64> = (1..=6).map(|n| schedule.wait_ms(n)).collect();
        // Three 1s waits fill upTo 3s exactly (e + w == bound stays); the 10s
        // segment's first wait exceeds its own 5s bound, so it contributes
        // zero waits and the handoff cascades to the 250ms tail.
        assert_eq!(waits, [1_000, 1_000, 1_000, 250, 250, 250]);
    }

    #[test]
    fn schedule_composition_restarts_exp_and_tracks_jitter_per_segment() {
        let schedule: Schedule = "fixed(1s) upTo 2s andThen exp(100ms, x3, jitter)"
            .parse()
            .unwrap();
        // exp restarts at local attempt 1 after the handoff.
        let waits: Vec<u64> = (1..=5).map(|n| schedule.wait_ms(n)).collect();
        assert_eq!(waits, [1_000, 1_000, 100, 300, 900]);
        // jitter is the emitting segment's flag, not schedule-global.
        assert_eq!(schedule.wait_and_jitter(1), (1_000, false));
        assert_eq!(schedule.wait_and_jitter(3), (100, true));
    }

    #[test]
    fn schedule_composition_shape_rule_rejections() {
        // Grammatical but invalid shapes fail configure-time (KEEL-E001), per
        // conformance/README.md "Schedule algebra": a non-final segment
        // without upTo never hands off; a bounded final segment would leave
        // attempts without a wait.
        for degenerate in [
            "fixed(1s) andThen fixed(2s)",
            "exp(1s, x2, max 5m) upTo 10m",
            "fixed(1s) upTo 3s andThen fixed(2s) andThen fixed(4s)",
            "fixed(1s) upTo 3s andThen fixed(2s) upTo 5s",
        ] {
            let error = degenerate.parse::<Schedule>().unwrap_err();
            assert!(
                error.to_string().contains("upTo"),
                "{degenerate}: expected the shape-rule note, got {error}"
            );
        }
        // Broken composition syntax stays a plain parse rejection.
        for broken in [
            "fixed(1s) upTo",
            "upTo 3s andThen fixed(1s)",
            "fixed(1s) upTo 1s upTo 2s andThen fixed(1s)",
            "fixed(1s) andThen",
            "andThen fixed(1s)",
            "fixed(1s) upTo 3s fixed(2s)",
        ] {
            assert!(
                broken.parse::<Schedule>().is_err(),
                "{broken} must be rejected"
            );
        }
    }

    #[test]
    fn condition_matching() {
        let on = RetryPolicy::default_on();
        let matches = |class, status| on.iter().any(|c| c.matches(class, status));
        assert!(matches(ErrorClass::Conn, None));
        assert!(matches(ErrorClass::Http, Some(429)));
        assert!(matches(ErrorClass::Http, Some(503)));
        assert!(!matches(ErrorClass::Http, Some(400)));
        assert!(!matches(ErrorClass::Cancelled, None));
        assert!("teapot".parse::<Condition>().is_err());
        // Exact-status literals follow the frozen schema grammar [1-5][0-9][0-9].
        assert_eq!("429".parse::<Condition>(), Ok(Condition::Status(429)));
        assert_eq!("100".parse::<Condition>(), Ok(Condition::Status(100)));
        assert_eq!("599".parse::<Condition>(), Ok(Condition::Status(599)));
        for bad in ["999", "099", "600", "000", "12", "1234", "1x9"] {
            assert!(bad.parse::<Condition>().is_err(), "{bad} must be rejected");
        }
    }

    #[test]
    fn zero_attempts_is_unrepresentable() {
        let doc = json!({ "target": { "x": { "retry": { "attempts": 0 } } } });
        let err = serde_path_to_error::deserialize::<_, Policy>(&doc).unwrap_err();
        assert_eq!(err.path().to_string(), "target.x.retry.attempts");
    }

    #[test]
    fn breaker_failure_rate_range_is_enforced() {
        // Frozen schema: breaker.failure_rate is exclusiveMinimum 0, maximum 1.
        let bad = |rate: serde_json::Value| {
            let doc = json!({ "target": { "x": { "breaker": { "failure_rate": rate } } } });
            serde_path_to_error::deserialize::<_, Policy>(&doc)
        };
        for rate in [json!(0.0), json!(-0.1), json!(1.5), json!(2.0)] {
            let err = bad(rate.clone()).unwrap_err();
            assert_eq!(
                err.path().to_string(),
                "target.x.breaker.failure_rate",
                "out-of-range failure_rate {rate} must fail at its path"
            );
        }
        // In-range values (0, 1] deserialize fine (paired with `window`:
        // rate mode requires both knobs).
        for rate in [0.01_f64, 0.5, 1.0] {
            let doc = json!({
                "target": { "x": { "breaker": { "window": "30s", "failure_rate": rate } } }
            });
            let policy = serde_path_to_error::deserialize::<_, Policy>(&doc).unwrap();
            let breaker = policy.target["x"].breaker.as_ref().unwrap();
            assert_eq!(breaker.failure_rate, Some(rate));
        }
    }

    #[test]
    fn breaker_mode_selection_follows_the_schema() {
        let breaker = |doc: serde_json::Value| -> BreakerPolicy {
            let doc = json!({ "target": { "x": { "breaker": doc } } });
            let policy: Policy = serde_path_to_error::deserialize(&doc).unwrap();
            policy.target["x"].breaker.clone().unwrap()
        };

        // Empty table: count mode on the schema default (failures = 5).
        assert_eq!(
            breaker(json!({})).mode(),
            BreakerMode::Count {
                failures: BreakerPolicy::DEFAULT_FAILURES
            }
        );

        // Both rate knobs, no `failures`: rate mode, min_calls defaults to 10.
        assert_eq!(
            breaker(json!({ "window": "30s", "failure_rate": 0.5 })).mode(),
            BreakerMode::Rate {
                window: DurationMs(30_000),
                failure_rate: 0.5,
                min_calls: BreakerPolicy::DEFAULT_MIN_CALLS,
            }
        );
        assert_eq!(
            breaker(json!({ "window": "10s", "failure_rate": 1.0, "min_calls": 4 })).mode(),
            BreakerMode::Rate {
                window: DurationMs(10_000),
                failure_rate: 1.0,
                min_calls: NonZeroU32::new(4).unwrap(),
            }
        );

        // "Setting `failures` selects count mode" (frozen schema): rate knobs
        // present alongside it are inert, and the policy says so.
        let mixed = breaker(json!({ "failures": 3, "window": "30s", "failure_rate": 0.5 }));
        assert_eq!(
            mixed.mode(),
            BreakerMode::Count {
                failures: NonZeroU64::new(3).unwrap()
            }
        );
        assert!(mixed.has_inert_rate_knobs());
        assert!(!breaker(json!({ "failures": 3 })).has_inert_rate_knobs());
    }

    #[test]
    fn half_configured_breaker_rate_mode_is_rejected() {
        // A rate-mode knob without both `window` and `failure_rate` (and
        // without `failures`) must fail at configure, not silently run count
        // mode the user never asked for.
        for doc in [
            json!({ "window": "30s" }),
            json!({ "failure_rate": 0.5 }),
            json!({ "min_calls": 10 }),
            json!({ "window": "30s", "min_calls": 10 }),
            json!({ "failure_rate": 0.5, "min_calls": 10 }),
        ] {
            let policy = json!({ "target": { "x": { "breaker": doc } } });
            let err = serde_path_to_error::deserialize::<_, Policy>(&policy).unwrap_err();
            assert_eq!(err.path().to_string(), "target.x.breaker", "doc: {doc}");
            assert!(
                err.inner().to_string().contains("rate mode requires both"),
                "doc {doc}: got {}",
                err.inner()
            );
        }
        // `failures` present makes any knob combination count mode (schema
        // precedence), so those documents stay valid.
        let policy =
            json!({ "target": { "x": { "breaker": { "failures": 3, "window": "30s" } } } });
        assert!(serde_path_to_error::deserialize::<_, Policy>(&policy).is_ok());
    }

    #[test]
    fn unknown_key_is_rejected_with_its_path() {
        // A typo'd nested key: the frozen schema's additionalProperties:false and
        // E001's "an unknown key was used" mean this must fail, not silently run
        // on the defaults.
        let doc = json!({ "target": { "api.stripe.com": { "retry": { "atempts": 10 } } } });
        let err = serde_path_to_error::deserialize::<_, Policy>(&doc).unwrap_err();
        assert!(
            err.inner().to_string().contains("atempts")
                || err.inner().to_string().contains("unknown field"),
            "expected an unknown-field error, got {}",
            err.inner()
        );
    }

    #[test]
    fn unknown_top_level_and_layer_keys_are_rejected() {
        assert!(
            serde_path_to_error::deserialize::<_, Policy>(&json!({ "bogus_top": true })).is_err()
        );
        assert!(
            serde_path_to_error::deserialize::<_, Policy>(
                &json!({ "target": { "api.x": { "retrys": {} } } })
            )
            .is_err(),
            "a mistyped layer table must be rejected, not dropped"
        );
    }

    #[test]
    fn journal_and_telemetry_parse_and_validate() {
        let doc = json!({
            "journal": "file:/srv/keel/journal.db",
            "telemetry": { "otlp_endpoint": "http://collector:4317" }
        });
        let policy: Policy = serde_path_to_error::deserialize(&doc).unwrap();
        assert_eq!(
            policy.journal.unwrap(),
            JournalLocation("file:/srv/keel/journal.db".to_owned())
        );
        let telemetry = policy.telemetry.unwrap();
        assert_eq!(
            telemetry.otlp_endpoint.as_deref(),
            Some("http://collector:4317")
        );
        assert!(telemetry.console, "schema default console = true");

        // A journal string that matches neither `file:` nor `postgres://` fails.
        let bad = json!({ "journal": "sqlite:/tmp/x.db" });
        assert!(serde_path_to_error::deserialize::<_, Policy>(&bad).is_err());
    }

    #[test]
    fn idempotency_resolves_like_any_other_layer() {
        // The `idempotency` layer must surface through `resolve()` so adapters
        // (via the front ends) and the engine can honor the injection contract
        // (contracts/adapter-pack.md "Idempotency-key injection").
        let doc = json!({
            "defaults": {
                "outbound": { "idempotency": { "header": "X-Idem" } },
                "llm": { "idempotency": { "header": "X-Llm-Idem" } }
            },
            "target": {
                "api.stripe.com": { "idempotency": { "header": "Idempotency-Key" } },
                "api.plain.example": { "timeout": "1s" }
            }
        });
        let policy: Policy = serde_path_to_error::deserialize(&doc).unwrap();

        // Exact target entry wins.
        let stripe = policy.resolve("api.stripe.com");
        assert_eq!(
            stripe.idempotency.as_ref().map(|i| i.header.as_str()),
            Some("Idempotency-Key")
        );
        // llm:* falls to defaults.llm, then anything else to defaults.outbound.
        let llm = policy.resolve("llm:openai");
        assert_eq!(
            llm.idempotency.as_ref().map(|i| i.header.as_str()),
            Some("X-Llm-Idem")
        );
        let plain = policy.resolve("api.plain.example");
        assert_eq!(
            plain.idempotency.as_ref().map(|i| i.header.as_str()),
            Some("X-Idem")
        );
        // No idempotency anywhere: resolves to None.
        let empty: Policy = serde_path_to_error::deserialize(&json!({})).unwrap();
        assert!(empty.resolve("api.stripe.com").idempotency.is_none());
    }

    #[test]
    fn poll_policy_parses_and_resolves() {
        let policy: Policy = serde_json::from_value(serde_json::json!({
            "target": { "api.jobs.example": { "poll": {
                "interval": "10s", "deadline": "90s",
                "until": { "field": "status", "terminal": ["completed", "failed"] }
            } } }
        }))
        .expect("valid poll policy");
        let resolved = policy.resolve("api.jobs.example");
        let poll = resolved.poll.expect("poll resolved");
        assert_eq!(poll.interval.0, 10_000);
        assert_eq!(poll.deadline.0, 90_000);
        assert_eq!(poll.until.field, "status");
        assert_eq!(poll.until.terminal, vec!["completed", "failed"]);
    }

    #[test]
    fn poll_rejects_empty_terminal_and_empty_field() {
        for bad in [
            serde_json::json!({ "interval": "10s", "deadline": "90s",
                "until": { "field": "status", "terminal": [] } }),
            serde_json::json!({ "interval": "10s", "deadline": "90s",
                "until": { "field": "", "terminal": ["done"] } }),
            serde_json::json!({ "interval": "10s",
                "until": { "field": "status", "terminal": ["done"] } }),
        ] {
            let doc = serde_json::json!({ "target": { "x": { "poll": bad } } });
            assert!(serde_json::from_value::<Policy>(doc).is_err());
        }
    }

    #[test]
    fn poll_rejects_zero_interval() {
        let doc = serde_json::json!({ "target": { "x": { "poll": {
            "interval": "0ms", "deadline": "90s",
            "until": { "field": "status", "terminal": ["done"] }
        } } } });
        assert!(serde_json::from_value::<Policy>(doc).is_err());

        let doc = serde_json::json!({ "target": { "x": { "poll": {
            "interval": "1ms", "deadline": "90s",
            "until": { "field": "status", "terminal": ["done"] }
        } } } });
        assert!(serde_json::from_value::<Policy>(doc).is_ok());
    }

    #[test]
    fn layer_resolution_precedence() {
        let doc = json!({
            "defaults": {
                "outbound": { "retry": { "attempts": 3 }, "rate": "9/s" },
                "llm": { "retry": { "attempts": 6 } }
            },
            "target": { "llm:openai": { "cache": { "ttl": "10m" } } }
        });
        let policy: Policy = serde_path_to_error::deserialize(&doc).unwrap();

        // llm:* target: cache from its own entry, retry from defaults.llm,
        // rate falls through to defaults.outbound
        let llm = policy.resolve("llm:openai");
        assert_eq!(llm.cache.unwrap().ttl, Some(DurationMs(600_000)));
        assert_eq!(llm.retry.unwrap().attempts.get(), 6);
        assert_eq!(llm.rate.unwrap().limit.get(), 9);

        // plain target: everything from defaults.outbound
        let plain = policy.resolve("api.example.com");
        assert_eq!(plain.retry.unwrap().attempts.get(), 3);
        assert!(plain.cache.is_none());
    }

    #[test]
    fn flows_on_busy_parses_with_skip_default() {
        let p: Policy = serde_json::from_value(serde_json::json!({
            "flows": { "entrypoints": ["cmd:autonomous-run"], "on_busy": "wait" }
        }))
        .unwrap();
        assert_eq!(p.flows.as_ref().unwrap().on_busy, OnBusy::Wait);
        let p: Policy = serde_json::from_value(serde_json::json!({ "flows": {} })).unwrap();
        assert_eq!(p.flows.unwrap().on_busy, OnBusy::Skip);
    }
}
