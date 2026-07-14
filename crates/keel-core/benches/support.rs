//! Shared overhead-measurement scaffolding for the `overhead` criterion bench
//! and the `overhead_guard` debug tripwire. Both targets `include!` this module
//! (the bench via `mod support;`, the test via `#[path = "..."]`), so it defines
//! exactly the four cases and the one measurement helper each of them uses —
//! keeping a single definition of "the deployed per-call path" for both.
//!
//! The deployed path is a current-thread `Runtime::block_on(Engine::execute)`,
//! the same shape `keel-ffi`'s `keel_execute` drives, so what we measure is what
//! ships. The five cases isolate Keel's overhead from the runtime floor:
//!
//! - `0_baseline`  — `block_on` of a trivial future: the irreducible floor that
//!   is *not* Keel overhead (subtract it from the others).
//! - `a_empty`     — default (empty) policy: `resolve()` with every layer off,
//!   one effect call, settle. The minimum a wrapped call ever costs.
//! - `b_cache_miss`— realistic policy (retry + breaker + cache) with an
//!   `args_hash`, on the cache-miss path (consult → miss → effect → write).
//! - `c_cache_hit` — the same realistic policy, served from a warm cache entry
//!   (attempts stays 0; the effect is never invoked).
//! - `d_events`    — the `a_empty` call with a live event sink attached: the
//!   emit path (call_start/attempt_start/call_end per call) stays on budget.
//!   The sink drains to `io::sink()` so the OS filesystem is not what's timed —
//!   file I/O runs on the background writer thread in production anyway.

use std::hint::black_box;
use std::time::Instant;

use keel_core_api::{AttemptResult, ENVELOPE_VERSION, Request};
use keelrun_core::Engine;
use keelrun_core::events::EventSink;
use serde_json::Value;
use tokio::runtime::{Builder, Runtime};

/// The five overhead cases, each pre-built so a measured iteration performs only
/// the deployed per-call path — never setup. Engines are separate per case so a
/// miss-path write can never warm the hit-path cache (or vice versa).
#[derive(Debug)]
pub struct Cases {
    runtime: Runtime,
    empty_engine: Engine,
    miss_engine: Engine,
    hit_engine: Engine,
    events_engine: Engine,
    empty_request: Request,
    miss_request: Request,
    hit_request: Request,
    events_request: Request,
}

/// A realistic policy — retry + breaker + cache all configured with contract
/// defaults — scoped to `target`, with the given cache `ttl` literal.
fn realistic_policy(target: &str, ttl: &str) -> Value {
    serde_json::json!({
        "target": {
            target: {
                "retry": {},
                "breaker": {},
                "cache": { "ttl": ttl },
            }
        }
    })
}

/// A request for `target`; `args_hash` is `Some` only when the case needs the
/// cache key material (miss/hit), `None` for the cache-less empty case.
fn request(target: &str, args_hash: Option<&str>) -> Request {
    Request {
        v: ENVELOPE_VERSION,
        target: target.to_owned(),
        op: format!("GET {target}"),
        idempotent: true,
        args_hash: args_hash.map(str::to_owned),
    }
}

/// A no-op effect: one successful attempt returning a trivial payload. The
/// engine still runs the full chain and (on a miss) writes the payload through.
async fn ok_effect(_attempt: u32) -> AttemptResult {
    AttemptResult::Ok {
        payload: Value::Bool(true),
    }
}

impl Cases {
    /// Build every engine, apply the realistic policies, and warm the hit-path
    /// cache with one live call — all excluded from later measurement.
    #[must_use]
    pub fn new() -> Self {
        let runtime = Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("current-thread runtime with the time driver must build");

        // Empty engine keeps its default (empty) policy: defaults resolution only.
        let empty_engine = Engine::new();

        // Miss engine: cache ttl 0 makes every consult a fresh miss with a
        // constant key, so the map stays O(1) and the miss path is repeatable.
        let miss_engine = Engine::new();
        miss_engine
            .configure(&realistic_policy("bench.miss", "0ms"))
            .expect("bench miss policy must configure");

        // Hit engine: a long ttl and one warm-up call keep the entry live for
        // the whole run, so every measured call is served from cache.
        let hit_engine = Engine::new();
        hit_engine
            .configure(&realistic_policy("bench.hit", "1h"))
            .expect("bench hit policy must configure");

        // Events engine: the empty-policy call with a live sink, so the
        // measured delta over `a_empty` is exactly the emit path.
        let mut events_engine = Engine::new();
        events_engine.attach_events(
            EventSink::to_writer(Box::new(std::io::sink()), "bench")
                .expect("bench event sink must start"),
        );

        let empty_request = request("bench.empty", None);
        let miss_request = request("bench.miss", Some("args-0"));
        let hit_request = request("bench.hit", Some("args-0"));
        let events_request = request("bench.events", None);

        runtime.block_on(async {
            black_box(hit_engine.execute(&hit_request, ok_effect).await);
        });

        Self {
            runtime,
            empty_engine,
            miss_engine,
            hit_engine,
            events_engine,
            empty_request,
            miss_request,
            hit_request,
            events_request,
        }
    }

    /// Case `0_baseline`: the `block_on` floor with a trivial future.
    pub fn baseline(&self) {
        black_box(self.runtime.block_on(async { black_box(0_u32) }));
    }

    /// Case `a_empty`: a wrapped call under the default (empty) policy.
    pub fn empty(&self) {
        black_box(
            self.runtime
                .block_on(self.empty_engine.execute(&self.empty_request, ok_effect)),
        );
    }

    /// Case `b_cache_miss`: a wrapped call on the realistic cache-miss path.
    pub fn miss(&self) {
        black_box(
            self.runtime
                .block_on(self.miss_engine.execute(&self.miss_request, ok_effect)),
        );
    }

    /// Case `c_cache_hit`: a wrapped call served from a warm cache entry.
    pub fn hit(&self) {
        black_box(
            self.runtime
                .block_on(self.hit_engine.execute(&self.hit_request, ok_effect)),
        );
    }

    /// Case `d_events`: the empty-policy call with a live event sink attached.
    pub fn events(&self) {
        black_box(
            self.runtime
                .block_on(self.events_engine.execute(&self.events_request, ok_effect)),
        );
    }
}

impl Default for Cases {
    fn default() -> Self {
        Self::new()
    }
}

/// Median nanoseconds per invocation of `op`, over `SAMPLES` batches of `INNER`
/// calls each (median-of-batch-means: robust to scheduler blips, deterministic
/// enough for a tripwire and a published number). Integer-only, so no lossy
/// float casts and no clippy waivers.
#[must_use]
pub fn median_ns(mut op: impl FnMut()) -> u64 {
    const INNER: u32 = 256;
    const SAMPLES: usize = 101;

    for _ in 0..INNER {
        op();
    }
    let mut samples: Vec<u128> = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let start = Instant::now();
        for _ in 0..INNER {
            op();
        }
        samples.push(start.elapsed().as_nanos() / u128::from(INNER));
    }
    samples.sort_unstable();
    u64::try_from(samples[SAMPLES / 2]).unwrap_or(u64::MAX)
}
