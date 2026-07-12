//! Optional OpenTelemetry OTLP export for `keel-core`'s `tracing` spans
//! (`otel` feature, architecture spec §4.5).
//!
//! The engine emits every wrapped call as a `keel.call` span with a
//! `keel.attempt` child per attempt (see [`crate::Engine`]). This module is the
//! only place those spans touch OpenTelemetry: [`init_otlp`] builds an OTLP
//! exporter, wires a batch [`SdkTracerProvider`] through
//! [`tracing_opentelemetry`], and installs it as the global `tracing`
//! subscriber. Without the `otel` feature the core has no OpenTelemetry
//! dependency at all, and even with it, spans stay near-free until `init_otlp`
//! runs.
//!
//! **Configuration is environment-driven.** Keel adds no telemetry surface to
//! `keel.toml` (the policy contract is frozen). Every standard `OTEL_*`
//! variable applies to the exporter — `OTEL_EXPORTER_OTLP_ENDPOINT`,
//! `OTEL_EXPORTER_OTLP_HEADERS`, `OTEL_EXPORTER_OTLP_PROTOCOL`,
//! `OTEL_EXPORTER_OTLP_TIMEOUT`, `OTEL_SERVICE_NAME`, `OTEL_RESOURCE_ATTRIBUTES`,
//! … The `endpoint` argument, when `Some`, overrides
//! `OTEL_EXPORTER_OTLP_ENDPOINT`; pass `None` to defer entirely to the
//! environment.
//!
//! Call `init_otlp` once, early, from within a Tokio runtime; hold the returned
//! [`OtelGuard` ] for the lifetime of the process so buffered spans flush on
//! shutdown.

use core::fmt;

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::{SpanExporter, WithExportConfig as _};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing_subscriber::Registry;
use tracing_subscriber::layer::SubscriberExt as _;

/// The service name stamped on exported spans when neither `OTEL_SERVICE_NAME`
/// nor `OTEL_RESOURCE_ATTRIBUTES` overrides it.
const DEFAULT_SERVICE_NAME: &str = "keel";

/// Owns the exporter for the lifetime of telemetry export. On drop it shuts the
/// [`SdkTracerProvider`] down, flushing any spans still buffered by the batch
/// processor so none are lost at process exit. Dropping it does **not** remove
/// the global subscriber (that lives for the whole process by design).
#[derive(Debug)]
pub struct OtelGuard {
    provider: SdkTracerProvider,
}

impl Drop for OtelGuard {
    fn drop(&mut self) {
        if let Err(error) = self.provider.shutdown() {
            // Telemetry never gets to break the process: a failed flush is a
            // warning, not a panic.
            tracing::warn!(%error, "otel exporter shutdown failed; buffered spans may be lost");
        }
    }
}

/// Why wiring OTLP export failed.
#[derive(Debug)]
#[non_exhaustive]
pub enum OtelInitError {
    /// The OTLP span exporter could not be built (endpoint, transport, or TLS
    /// misconfiguration).
    Exporter(opentelemetry_otlp::ExporterBuildError),
    /// A global `tracing` subscriber was already installed by this process, so
    /// Keel could not install the OpenTelemetry bridge.
    Subscriber(tracing::subscriber::SetGlobalDefaultError),
}

impl fmt::Display for OtelInitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exporter(error) => write!(f, "failed to build the OTLP span exporter: {error}"),
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

/// Install OTLP export for `keel-core`'s `tracing` spans and return a guard that
/// flushes on drop.
///
/// `endpoint` overrides `OTEL_EXPORTER_OTLP_ENDPOINT` when `Some`; pass `None`
/// to take the endpoint (and all other settings) from the standard `OTEL_*`
/// environment variables. Must be called at most once per process — a second
/// call, or a call after another subscriber is installed, returns
/// [`OtelInitError::Subscriber`].
pub fn init_otlp(endpoint: Option<&str>) -> Result<OtelGuard, OtelInitError> {
    let mut builder = SpanExporter::builder().with_tonic();
    if let Some(endpoint) = endpoint {
        builder = builder.with_endpoint(endpoint);
    }
    let exporter = builder.build()?;

    let resource = Resource::builder()
        .with_service_name(DEFAULT_SERVICE_NAME)
        .build();
    let provider = SdkTracerProvider::builder()
        .with_resource(resource)
        .with_batch_exporter(exporter)
        .build();

    let tracer = provider.tracer("keel-core");
    let layer = tracing_opentelemetry::layer().with_tracer(tracer);
    let subscriber = Registry::default().with(layer);
    tracing::subscriber::set_global_default(subscriber)?;

    Ok(OtelGuard { provider })
}
