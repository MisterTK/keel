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
//! Tier 2 (journal, flows, replay) and the FFI facade land in later slices;
//! the async surface here is what the PyO3/napi bridges will wrap.

mod engine;

pub use engine::{DiscoveryRecorder, Engine};
