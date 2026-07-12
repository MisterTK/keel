//! keel-core: the real Keel kernel, Tier 1 scope (architecture spec §4).
//!
//! One [`Engine`] per process runs every intercepted call through its
//! target's layer chain — cache → rate → breaker → timeout → retry — driven
//! by the shared typed policy model ([`keel_core_api::policy`]). Waits are
//! real `tokio::time` sleeps: production sleeps wall-clock time, and the
//! conformance harness runs under `start_paused` where the same sleeps
//! advance virtual time deterministically. The engine passes the identical
//! scenario corpus as `keel-core-stub` — that equivalence is the whole point
//! of the conformance suite.
//!
//! Beyond the stub, the real engine:
//! - enforces `timeout` with `tokio::time::timeout` around each attempt
//!   (a policy-layer timeout terminates as `KEEL-E011`)
//! - applies schedule `jitter` (equal jitter: uniform in `[w/2, w]`)
//! - is `&self`-concurrent: interior state behind a mutex, never held
//!   across an await
//!
//! Every call and attempt is also emitted as a `tracing` span
//! (`keel.call` / `keel.attempt`, architecture spec §4.5), with breaker
//! transitions and cache hits as debug events. Spans cost effectively
//! nothing when no subscriber is active. Enabling the optional `otel`
//! feature adds [`otel::init_otlp`] to export those spans over OTLP.
//! Independently of tracing, the [`events`] sink streams a live NDJSON
//! feed of attempts/backoffs/breaker transitions to `.keel/events/` (for
//! `keel tail`), and mints the trace refs Tier 1 failure messages carry.
//!
//! Tier 2 durable flows ([`FlowManager`]) build on the Tier 1 engine and the
//! [`keel_journal`] persistence layer; the FFI facade wraps this async surface
//! (later slices) for the PyO3/napi bridges.

mod engine;
pub mod events;
mod flow;
#[cfg(feature = "otel")]
pub mod otel;

pub use engine::{DiscoveryRecorder, Engine};
pub use flow::{FlowConfig, FlowDescriptor, FlowHandle, FlowManager};
