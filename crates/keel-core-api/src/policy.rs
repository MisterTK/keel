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
}

impl ParseError {
    fn new(what: &'static str, input: &str) -> Self {
        Self {
            what,
            input: input.to_owned(),
        }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unparseable {} literal: {:?}", self.what, self.input)
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

/// A retry schedule per contracts/schedule-grammar.ebnf. The stub implements
/// the v0.1 primaries (`exp`, `fixed`); composition (`upTo`/`andThen`) is in
/// the frozen grammar but rejected here, so using it is a configure-time
/// `KEEL-E001` rather than a silent misbehavior.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(try_from = "String")]
pub enum Schedule {
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

impl Schedule {
    /// The contract default: `exp(200ms, x2, max 30s, jitter)`.
    pub const DEFAULT: Self = Self::Exp {
        base_ms: 200,
        factor: 2.0,
        cap_ms: 30_000,
        jitter: true,
    };

    /// True when the schedule requests jitter. The stub ignores it (its
    /// clock is virtual and deterministic); the real core samples equal
    /// jitter, uniform in `[w/2, w]`, per contracts/schedule-grammar.ebnf.
    #[must_use]
    pub fn has_jitter(self) -> bool {
        matches!(self, Self::Exp { jitter: true, .. })
    }

    /// Deterministic wait after failed attempt `n` (1-based):
    /// `min(base * factor^(n-1), cap)`, before any jitter.
    #[expect(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_possible_wrap,
        reason = "backoff arithmetic: values are small and non-negative by construction"
    )]
    pub fn wait_ms(self, attempt: u32) -> u64 {
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
}

impl FromStr for Schedule {
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
            schedule: Schedule::DEFAULT,
            on: Self::default_on(),
        }
    }
}

/// `breaker = { failures, cooldown, ... }`. The stub implements count mode
/// (`failures` consecutive terminal failures); the rate-mode knobs are
/// accepted and validated per the schema but enforced only by the real core.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BreakerPolicy {
    pub failures: NonZeroU64,
    pub cooldown: DurationMs,
    pub window: Option<DurationMs>,
    #[serde(default, deserialize_with = "de_failure_rate")]
    pub failure_rate: Option<f64>,
    pub min_calls: Option<NonZeroU32>,
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
            failures: NonZeroU64::new(5).unwrap(),
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

/// Tier 2 flow designation — parsed and carried, enforced by the real core.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FlowsPolicy {
    pub entrypoints: Vec<String>,
    pub on_nondeterminism: NondeterminismResponse,
}

/// A journal location literal (`policy.journal`), validated against the frozen
/// schema pattern `^(file:.+|postgres://.+)$` at parse time so a malformed value
/// fails configuration (KEEL-E001) rather than being silently ignored. Parsed
/// and carried; the concrete path is selected by the front end / core at
/// construction (KEEL_JOURNAL or the `.keel/journal.db` default) in v0.1.
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

/// `[telemetry]` (`otlp_endpoint`, `console`). Parsed and carried, but inert in
/// v0.1: OTel export is configured from the environment (see `keel-core`'s otel
/// module), so setting this only validates — `Engine::configure` warns when it
/// is present so the user is not silently surprised.
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
    /// Journal location (schema-validated). Parsed and carried; path selection
    /// is construction-time in v0.1 (see [`JournalLocation`]).
    pub journal: Option<JournalLocation>,
    /// Telemetry config (schema-validated). Parsed and carried but inert in
    /// v0.1 (env-driven export); see [`TelemetryPolicy`].
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
}

impl Policy {
    pub fn resolve(&self, target: &str) -> ResolvedPolicy {
        ResolvedPolicy {
            timeout: self.layer(target, |t| t.timeout.as_ref()).copied(),
            retry: self.layer(target, |t| t.retry.as_ref()).cloned(),
            breaker: self.layer(target, |t| t.breaker.as_ref()).cloned(),
            rate: self.layer(target, |t| t.rate.as_ref()).copied(),
            cache: self.layer(target, |t| t.cache.as_ref()).cloned(),
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
            Ok(Schedule::Fixed { period_ms: 1_000 })
        );
        // frozen grammar, unimplemented primary composition -> configure error
        assert!("exp(1s, x2) andThen fixed(1m)".parse::<Schedule>().is_err());
        assert!("linear(1s)".parse::<Schedule>().is_err());
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
        // In-range values (0, 1] deserialize fine.
        for rate in [0.01_f64, 0.5, 1.0] {
            let doc = json!({ "target": { "x": { "breaker": { "failure_rate": rate } } } });
            let policy = serde_path_to_error::deserialize::<_, Policy>(&doc).unwrap();
            let breaker = policy.target["x"].breaker.as_ref().unwrap();
            assert_eq!(breaker.failure_rate, Some(rate));
        }
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
}
