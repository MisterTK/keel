//! In-process metric emission (architecture spec §4.5): attempt counts, retry
//! counts + backoff waits, cache hit ratio, rate-limit throttling, breaker
//! transitions, and Tier 2 flow resume/recovery counts.
//!
//! Without the `otel` cargo feature every function here is an empty no-op and
//! the module pulls **zero** dependencies — the default build stays
//! OpenTelemetry-free (the CLAUDE.md opt-in decision). With the feature, each
//! function records onto OTel instruments that [`bind_global_meter`] binds
//! (once per process) against the *currently installed* global meter provider;
//! [`crate::otel::init_otlp`] calls it right after installing the OTLP meter
//! provider. Until instruments are bound, every recording call is a single
//! atomic load (`OnceLock::get` → `None`), so the disabled-telemetry fast path
//! the overhead budget protects is unperturbed — mirroring how spans stay
//! near-free until a subscriber is installed.
//!
//! Instrument names are `keel.`-prefixed and dot-namespaced per OTel semantic
//! conventions; every Tier 1 instrument carries a `keel.target` attribute so
//! per-target rates/ratios come out of a standard OTel backend without views:
//!
//! | instrument                 | kind          | attributes                          |
//! |----------------------------|---------------|-------------------------------------|
//! | `keel.attempts`            | counter (u64) | `keel.target`                       |
//! | `keel.retries`             | counter (u64) | `keel.target`                       |
//! | `keel.retry.backoff`       | histogram, ms | `keel.target`                       |
//! | `keel.cache.requests`      | counter (u64) | `keel.target`, `keel.cache.hit`     |
//! | `keel.rate.throttled`      | counter (u64) | `keel.target`                       |
//! | `keel.rate.wait`           | histogram, ms | `keel.target`                       |
//! | `keel.breaker.transitions` | counter (u64) | `keel.target`, `keel.breaker.transition` |
//! | `keel.flow.resumes`        | counter (u64) | `keel.flow.entrypoint`              |
//!
//! Cache hit *ratio* is derived downstream as
//! `keel.cache.requests{keel.cache.hit=true} / keel.cache.requests` — OTel
//! counters compose; a pre-divided gauge would not.

#[cfg(feature = "otel")]
mod enabled {
    use core::fmt;
    use std::sync::OnceLock;

    use opentelemetry::KeyValue;
    use opentelemetry::metrics::{Counter, Histogram, Meter};

    /// Instrumentation scope, matching the span side's tracer name.
    const METER_NAME: &str = "keel-core";
    /// The per-target attribute every Tier 1 instrument carries.
    const TARGET_KEY: &str = "keel.target";

    /// The bound instruments, created once against the global meter provider.
    struct Instruments {
        attempts: Counter<u64>,
        retries: Counter<u64>,
        retry_backoff: Histogram<f64>,
        cache_requests: Counter<u64>,
        throttled: Counter<u64>,
        rate_wait: Histogram<f64>,
        breaker_transitions: Counter<u64>,
        flow_resumes: Counter<u64>,
    }

    impl fmt::Debug for Instruments {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            // The OTel instrument handles aren't Debug; presence is the story.
            f.debug_struct("Instruments").finish_non_exhaustive()
        }
    }

    impl Instruments {
        fn build(meter: &Meter) -> Self {
            Self {
                attempts: meter
                    .u64_counter("keel.attempts")
                    .with_unit("{attempt}")
                    .with_description("Attempts executed (one per try, including the first)")
                    .build(),
                retries: meter
                    .u64_counter("keel.retries")
                    .with_unit("{retry}")
                    .with_description("Retries scheduled after a retryable failed attempt")
                    .build(),
                retry_backoff: meter
                    .f64_histogram("keel.retry.backoff")
                    .with_unit("ms")
                    .with_description("Backoff wait before each retry (post-jitter, ms)")
                    .build(),
                cache_requests: meter
                    .u64_counter("keel.cache.requests")
                    .with_unit("{request}")
                    .with_description(
                        "Cache lookups; hit ratio = sum(keel.cache.hit=true) / sum(all)",
                    )
                    .build(),
                throttled: meter
                    .u64_counter("keel.rate.throttled")
                    .with_unit("{call}")
                    .with_description("Calls delayed by the rate limiter (never failed)")
                    .build(),
                rate_wait: meter
                    .f64_histogram("keel.rate.wait")
                    .with_unit("ms")
                    .with_description("Rate-limit delay per throttled call (ms)")
                    .build(),
                breaker_transitions: meter
                    .u64_counter("keel.breaker.transitions")
                    .with_unit("{transition}")
                    .with_description(
                        "Breaker state changes (keel.breaker.transition: opened | half_open | closed)",
                    )
                    .build(),
                flow_resumes: meter
                    .u64_counter("keel.flow.resumes")
                    .with_unit("{resume}")
                    .with_description(
                        "Tier 2 flow recoveries: re-entries of an incomplete flow (attempt >= 2)",
                    )
                    .build(),
            }
        }
    }

    static INSTRUMENTS: OnceLock<Instruments> = OnceLock::new();

    /// Bind the engine's instruments to the **currently installed** global
    /// meter provider. Called by [`crate::otel::init_otlp`] right after it
    /// installs the OTLP meter provider; idempotent (first bind wins), and
    /// before the first call every recording hook is a no-op.
    ///
    /// Exposed (re-exported as `otel::bind_global_meter`) for embedders and
    /// tests that install their own `MeterProvider` instead of Keel's OTLP one.
    pub fn bind_global_meter() {
        let meter = opentelemetry::global::meter(METER_NAME);
        let _already_bound = INSTRUMENTS.set(Instruments::build(&meter));
    }

    fn get() -> Option<&'static Instruments> {
        INSTRUMENTS.get()
    }

    #[expect(
        clippy::cast_precision_loss,
        reason = "waits are milliseconds; f64 is exact for any wait a process could survive"
    )]
    fn ms(wait_ms: u64) -> f64 {
        wait_ms as f64
    }

    /// One attempt executed for `target` (including the first try of a call).
    pub fn record_attempt(target: &str) {
        if let Some(i) = get() {
            i.attempts.add(1, &[KeyValue::new(TARGET_KEY, target.to_owned())]);
        }
    }

    /// One retry scheduled for `target`, waiting `wait_ms` (post-jitter).
    pub fn record_retry(target: &str, wait_ms: u64) {
        if let Some(i) = get() {
            let attrs = [KeyValue::new(TARGET_KEY, target.to_owned())];
            i.retries.add(1, &attrs);
            i.retry_backoff.record(ms(wait_ms), &attrs);
        }
    }

    /// One cache lookup for `target`; `hit` is whether a fresh entry served it.
    pub fn record_cache_request(target: &str, hit: bool) {
        if let Some(i) = get() {
            i.cache_requests.add(
                1,
                &[
                    KeyValue::new(TARGET_KEY, target.to_owned()),
                    KeyValue::new("keel.cache.hit", hit),
                ],
            );
        }
    }

    /// One call delayed `wait_ms` by `target`'s rate limiter.
    pub fn record_throttled(target: &str, wait_ms: u64) {
        if let Some(i) = get() {
            let attrs = [KeyValue::new(TARGET_KEY, target.to_owned())];
            i.throttled.add(1, &attrs);
            i.rate_wait.record(ms(wait_ms), &attrs);
        }
    }

    /// One breaker state change on `target`; `transition` is the `snake_case`
    /// label the debug event/report also uses (`opened`/`half_open`/`closed`).
    pub fn record_breaker_transition(target: &str, transition: &'static str) {
        if let Some(i) = get() {
            i.breaker_transitions.add(
                1,
                &[
                    KeyValue::new(TARGET_KEY, target.to_owned()),
                    KeyValue::new("keel.breaker.transition", transition),
                ],
            );
        }
    }

    /// One Tier 2 flow recovery: an incomplete flow re-entered (attempt >= 2).
    pub fn record_flow_resume(entrypoint: &str) {
        if let Some(i) = get() {
            i.flow_resumes.add(
                1,
                &[KeyValue::new("keel.flow.entrypoint", entrypoint.to_owned())],
            );
        }
    }
}

#[cfg(feature = "otel")]
pub(crate) use enabled::*;

/// No-op twins: without the `otel` feature the hooks compile to nothing, so
/// call sites in the engine/flow manager stay clean of `cfg` noise.
#[cfg(not(feature = "otel"))]
mod disabled {
    /// No-op (`otel` feature off).
    pub fn record_attempt(_target: &str) {}
    /// No-op (`otel` feature off).
    pub fn record_retry(_target: &str, _wait_ms: u64) {}
    /// No-op (`otel` feature off).
    pub fn record_cache_request(_target: &str, _hit: bool) {}
    /// No-op (`otel` feature off).
    pub fn record_throttled(_target: &str, _wait_ms: u64) {}
    /// No-op (`otel` feature off).
    pub fn record_breaker_transition(_target: &str, _transition: &'static str) {}
    /// No-op (`otel` feature off).
    pub fn record_flow_resume(_entrypoint: &str) {}
}

#[cfg(not(feature = "otel"))]
pub(crate) use disabled::*;
