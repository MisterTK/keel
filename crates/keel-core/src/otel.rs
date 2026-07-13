//! Optional OpenTelemetry OTLP export for `keel-core`'s `tracing` spans **and
//! metrics** (`otel` feature, architecture spec §4.5).
//!
//! The engine emits every wrapped call as a `keel.call` span with a
//! `keel.attempt` child per attempt (see [`crate::Engine`]), and records the
//! §4.5 metric set — attempts, retries + backoff waits, cache hit ratio,
//! rate-limit throttling, breaker transitions, flow resumes — through the
//! hooks in the crate's `metrics` module. This module is the only place either
//! touches OpenTelemetry: [`init_otlp`] builds OTLP span *and* metric
//! exporters, wires a batch [`SdkTracerProvider`] through
//! [`tracing_opentelemetry`] plus a periodic [`SdkMeterProvider`], installs
//! them globally, and binds the engine's instruments
//! ([`bind_global_meter`]). Without the `otel` feature the core has no
//! OpenTelemetry dependency at all, and even with it, spans and metric hooks
//! stay near-free until `init_otlp` runs.
//!
//! **Configuration: environment first, `keel.toml` second (env wins).** Every
//! standard `OTEL_*` variable applies to the exporters —
//! `OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_EXPORTER_OTLP_HEADERS`,
//! `OTEL_EXPORTER_OTLP_PROTOCOL`, `OTEL_EXPORTER_OTLP_TIMEOUT`,
//! `OTEL_SERVICE_NAME`, `OTEL_RESOURCE_ATTRIBUTES`, the per-signal
//! `OTEL_EXPORTER_OTLP_{TRACES,METRICS}_*` twins, … The policy schema's
//! `[telemetry]` table supplies the same opt-in from `keel.toml` (spec §4.5:
//! "standard OTLP export, configured in — where else — keel.toml"): the
//! bindings read the *effective* policy's `telemetry.otlp_endpoint` and pass
//! it here. Precedence is twelve-factor:
//!
//! - `KEEL_OTEL` is the explicit gate: a truthy value (anything but
//!   `0`/`false`/`no`/`off`) forces export on, a falsy value forces it off,
//!   unset defers to the policy ([`export_enabled`]).
//! - `OTEL_EXPORTER_OTLP_ENDPOINT` (when set non-empty) beats
//!   `telemetry.otlp_endpoint` ([`resolve_endpoint`]).
//! - `telemetry.console` never reaches this module: it is the front ends'
//!   local pretty-summary switch, not an export knob.
//!
//! The `endpoint` argument of [`init_otlp`], when `Some`, overrides
//! `OTEL_EXPORTER_OTLP_ENDPOINT` for both signals; pass `None` to defer
//! entirely to the environment.
//!
//! Call `init_otlp` once, early, from within a Tokio runtime; hold the returned
//! [`OtelGuard`] for the lifetime of the process so buffered spans and the
//! final metric collection flush on shutdown.

use core::fmt;
use std::env;

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::{MetricExporter, SpanExporter, WithExportConfig as _};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::metrics::SdkMeterProvider;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing_subscriber::Registry;
use tracing_subscriber::layer::SubscriberExt as _;

pub use crate::metrics::bind_global_meter;

/// The service name stamped on exported spans/metrics when neither
/// `OTEL_SERVICE_NAME` nor `OTEL_RESOURCE_ATTRIBUTES` overrides it.
const DEFAULT_SERVICE_NAME: &str = "keel";

/// The explicit on/off gate for OTLP export.
const GATE_VAR: &str = "KEEL_OTEL";

/// The standard OTLP endpoint variable — set non-empty, it wins over policy.
const ENDPOINT_VAR: &str = "OTEL_EXPORTER_OTLP_ENDPOINT";

/// Owns the exporters for the lifetime of telemetry export. On drop it shuts
/// both providers down, flushing any spans still buffered by the batch
/// processor and running one final metric collection so nothing is lost at
/// process exit. Dropping it does **not** remove the global subscriber or
/// meter provider (those live for the whole process by design).
#[derive(Debug)]
pub struct OtelGuard {
    tracer_provider: SdkTracerProvider,
    meter_provider: SdkMeterProvider,
}

impl Drop for OtelGuard {
    fn drop(&mut self) {
        // Telemetry never gets to break the process: a failed flush is a
        // warning, not a panic.
        if let Err(error) = self.tracer_provider.shutdown() {
            tracing::warn!(%error, "otel span exporter shutdown failed; buffered spans may be lost");
        }
        if let Err(error) = self.meter_provider.shutdown() {
            tracing::warn!(%error, "otel metric exporter shutdown failed; the final collection may be lost");
        }
    }
}

/// Why wiring OTLP export failed.
#[derive(Debug)]
#[non_exhaustive]
pub enum OtelInitError {
    /// An OTLP exporter could not be built (endpoint, transport, or TLS
    /// misconfiguration).
    Exporter(opentelemetry_otlp::ExporterBuildError),
    /// A global `tracing` subscriber was already installed by this process, so
    /// Keel could not install the OpenTelemetry bridge.
    Subscriber(tracing::subscriber::SetGlobalDefaultError),
}

impl fmt::Display for OtelInitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exporter(error) => write!(f, "failed to build an OTLP exporter: {error}"),
            Self::Subscriber(error) => {
                write!(
                    f,
                    "a global tracing subscriber is already installed: {error}"
                )
            }
        }
    }
}

impl std::error::Error for OtelInitError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Exporter(error) => Some(error),
            Self::Subscriber(error) => Some(error),
        }
    }
}

impl From<opentelemetry_otlp::ExporterBuildError> for OtelInitError {
    fn from(error: opentelemetry_otlp::ExporterBuildError) -> Self {
        Self::Exporter(error)
    }
}

impl From<tracing::subscriber::SetGlobalDefaultError> for OtelInitError {
    fn from(error: tracing::subscriber::SetGlobalDefaultError) -> Self {
        Self::Subscriber(error)
    }
}

/// The `KEEL_OTEL` gate as a tri-state: `Some(true)` when set truthy (export
/// forced on, configured from the environment), `Some(false)` when set falsy
/// (`0`/`false`/`no`/`off` — export forced off, whatever policy says), `None`
/// when unset or blank (defer to policy). Case- and whitespace-insensitive.
#[must_use]
pub fn env_opt_in() -> Option<bool> {
    opt_in_from(env::var(GATE_VAR).ok().as_deref())
}

/// Whether OTLP export should initialize, given the effective policy's
/// `telemetry.otlp_endpoint`. Env wins: an explicit `KEEL_OTEL` decides alone;
/// unset, an endpoint in `keel.toml` is the opt-in (spec §4.5).
#[must_use]
pub fn export_enabled(policy_endpoint: Option<&str>) -> bool {
    env_opt_in().unwrap_or_else(|| policy_endpoint.is_some_and(|e| !e.trim().is_empty()))
}

/// The endpoint override to pass to [`init_otlp`], combining the environment
/// with the effective policy's `telemetry.otlp_endpoint`. Env wins: when
/// `OTEL_EXPORTER_OTLP_ENDPOINT` is set non-empty this returns `None` (the
/// exporter reads the environment itself); otherwise the policy endpoint, if
/// any, is returned as the override.
#[must_use]
pub fn resolve_endpoint(policy_endpoint: Option<&str>) -> Option<String> {
    endpoint_from(env::var(ENDPOINT_VAR).ok().as_deref(), policy_endpoint)
}

/// Pure core of [`env_opt_in`], unit-testable without touching process env.
fn opt_in_from(raw: Option<&str>) -> Option<bool> {
    let value = raw?.trim().to_ascii_lowercase();
    if value.is_empty() {
        return None;
    }
    Some(!matches!(value.as_str(), "0" | "false" | "no" | "off"))
}

/// Pure core of [`resolve_endpoint`], unit-testable without touching process env.
fn endpoint_from(env_endpoint: Option<&str>, policy_endpoint: Option<&str>) -> Option<String> {
    if env_endpoint.is_some_and(|e| !e.trim().is_empty()) {
        return None; // env wins; the exporter reads OTEL_EXPORTER_OTLP_ENDPOINT itself
    }
    policy_endpoint
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(str::to_owned)
}

/// Install OTLP export for `keel-core`'s `tracing` spans and §4.5 metrics, and
/// return a guard that flushes both on drop.
///
/// `endpoint` overrides `OTEL_EXPORTER_OTLP_ENDPOINT` (for both signals) when
/// `Some`; pass `None` to take the endpoint (and all other settings) from the
/// standard `OTEL_*` environment variables — [`resolve_endpoint`] computes the
/// right value from env + policy. Must be called at most once per process — a
/// second call, or a call after another subscriber is installed, returns
/// [`OtelInitError::Subscriber`] before any global state changes.
pub fn init_otlp(endpoint: Option<&str>) -> Result<OtelGuard, OtelInitError> {
    let mut span_builder = SpanExporter::builder().with_tonic();
    let mut metric_builder = MetricExporter::builder().with_tonic();
    if let Some(endpoint) = endpoint {
        span_builder = span_builder.with_endpoint(endpoint);
        metric_builder = metric_builder.with_endpoint(endpoint);
    }
    // Build both exporters before touching any global state, so a
    // misconfiguration cannot leave the process half-wired.
    let span_exporter = span_builder.build()?;
    let metric_exporter = metric_builder.build()?;

    let resource = Resource::builder()
        .with_service_name(DEFAULT_SERVICE_NAME)
        .build();
    let tracer_provider = SdkTracerProvider::builder()
        .with_resource(resource.clone())
        .with_batch_exporter(span_exporter)
        .build();

    // The subscriber install is the once-per-process gate: it fails on a
    // second call, so the meter provider below is also installed exactly once.
    let tracer = tracer_provider.tracer("keel-core");
    let layer = tracing_opentelemetry::layer().with_tracer(tracer);
    let subscriber = Registry::default().with(layer);
    tracing::subscriber::set_global_default(subscriber)?;

    let meter_provider = SdkMeterProvider::builder()
        .with_resource(resource)
        .with_periodic_exporter(metric_exporter)
        .build();
    opentelemetry::global::set_meter_provider(meter_provider.clone());
    // Bind the engine's instruments to the provider just installed; from here
    // on the metric hooks in engine.rs/flow.rs record for real.
    bind_global_meter();

    Ok(OtelGuard {
        tracer_provider,
        meter_provider,
    })
}

#[cfg(test)]
mod tests {
    use super::{endpoint_from, opt_in_from};

    #[test]
    fn opt_in_tristate() {
        // Unset / blank: defer to policy.
        assert_eq!(opt_in_from(None), None);
        assert_eq!(opt_in_from(Some("")), None);
        assert_eq!(opt_in_from(Some("   ")), None);
        // Explicit off, case/whitespace-insensitive.
        assert_eq!(opt_in_from(Some("0")), Some(false));
        assert_eq!(opt_in_from(Some("false")), Some(false));
        assert_eq!(opt_in_from(Some(" No ")), Some(false));
        assert_eq!(opt_in_from(Some("OFF")), Some(false));
        // Anything else set is on (back-compat: KEEL_OTEL=1 was "any set value").
        assert_eq!(opt_in_from(Some("1")), Some(true));
        assert_eq!(opt_in_from(Some("true")), Some(true));
        assert_eq!(opt_in_from(Some("yes")), Some(true));
        assert_eq!(opt_in_from(Some("collector-a")), Some(true));
    }

    #[test]
    fn endpoint_env_wins_over_policy() {
        // Env set non-empty: defer to the exporter's own env handling.
        assert_eq!(
            endpoint_from(Some("http://env:4317"), Some("http://policy:4317")),
            None
        );
        // Env unset/blank: the policy endpoint is the override.
        assert_eq!(
            endpoint_from(None, Some("http://policy:4317")),
            Some("http://policy:4317".to_owned())
        );
        assert_eq!(
            endpoint_from(Some("  "), Some(" http://policy:4317 ")),
            Some("http://policy:4317".to_owned())
        );
        // Neither: fully env-driven defaults.
        assert_eq!(endpoint_from(None, None), None);
        assert_eq!(endpoint_from(None, Some("  ")), None);
    }

    #[test]
    fn export_enabled_matrix() {
        use super::export_enabled;
        // No KEEL_OTEL in the test environment: policy decides. (The env-set
        // legs are covered by `opt_in_tristate` on the pure function — tests
        // must not mutate process env, it is shared across threads.)
        if std::env::var_os(super::GATE_VAR).is_none() {
            assert!(export_enabled(Some("http://collector:4317")));
            assert!(!export_enabled(Some("   ")));
            assert!(!export_enabled(None));
        }
    }
}
