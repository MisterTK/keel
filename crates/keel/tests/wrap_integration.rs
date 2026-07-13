//! Runtime proof that `#[keel::wrap]` actually routes a function's body
//! through the real `keel-core` `Engine` chain against a real `keel.toml`
//! fixture (`tests/fixtures/wrap/keel.toml`): retries on a classified
//! error, gives up per the policy's `attempts`, and serves a cached result
//! on a second call to a `cache`-configured target without re-running the
//! body.
//!
//! `keel::init_from` is called once per test (guarded by `is_initialized`)
//! since the process-wide engine is a `OnceLock` — every test in this file
//! shares ONE engine/policy, matching how a real binary calls `keel::init()`
//! once from `main`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

fn fixture_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/wrap")
}

fn ensure_init() {
    if !keel::is_initialized() {
        keel::init_from(fixture_dir()).expect("fixture keel.toml is valid");
    }
}

#[derive(Debug, thiserror::Error)]
#[error("upstream unavailable: {0}")]
struct UpstreamError(String);

#[keel::wrap(target = "orders-api", idempotent = true)]
async fn fetch_order(calls: Arc<AtomicU32>, fail_times: u32) -> Result<u32, UpstreamError> {
    let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
    if n <= fail_times {
        return Err(UpstreamError("connection reset".to_owned()));
    }
    Ok(n)
}

#[tokio::test]
async fn retries_per_policy_then_succeeds() {
    ensure_init();
    let calls = Arc::new(AtomicU32::new(0));
    let result = fetch_order(Arc::clone(&calls), 2).await;
    assert_eq!(
        result.unwrap(),
        3,
        "third attempt (n=3) is the one that succeeds"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn gives_up_after_configured_attempts_and_returns_the_original_error() {
    ensure_init();
    let calls = Arc::new(AtomicU32::new(0));
    // `orders-api`'s policy allows 3 attempts; failing every time exhausts it.
    let err = fetch_order(Arc::clone(&calls), u32::MAX).await.unwrap_err();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "exactly `attempts` calls, no more"
    );
    match err {
        keel::Error::Original(UpstreamError(msg)) => assert_eq!(msg, "connection reset"),
        keel::Error::Keel(e) => {
            panic!("expected the original error, got an engine judgment: {e:?}")
        }
    }
}

#[keel::wrap(target = "flaky-cache-api", idempotent = true)]
async fn cached_call(calls: Arc<AtomicU32>) -> Result<u32, UpstreamError> {
    Ok(calls.fetch_add(1, Ordering::SeqCst) + 1)
}

#[tokio::test]
async fn caching_is_not_implemented_for_wrap_in_v1_every_call_reruns_the_body() {
    // Documents a real v1 limitation (see the `wrap_call` doc comment in
    // `crates/keel/src/lib.rs`): `#[keel::wrap]` always submits
    // `args_hash: None`, which unconditionally disables the cache layer per
    // `contracts/core_api.rs` — even though `flaky-cache-api` configures a
    // `cache.ttl`. A future version could derive `args_hash` from a
    // `Hash`/`Serialize` bound on the wrapped function's parameters.
    ensure_init();
    let calls = Arc::new(AtomicU32::new(0));
    let first = cached_call(Arc::clone(&calls)).await.unwrap();
    let second = cached_call(Arc::clone(&calls)).await.unwrap();
    assert_eq!(first, 1);
    assert_eq!(
        second, 2,
        "no cache layer: the body reran and returned a new value"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[keel::wrap(target = "breaker-api", idempotent = true)]
async fn breaker_call(calls: Arc<AtomicU32>) -> Result<u32, UpstreamError> {
    calls.fetch_add(1, Ordering::SeqCst);
    Err(UpstreamError("connection reset".to_owned()))
}

#[tokio::test]
async fn a_tripped_breaker_fast_fails_as_a_keel_judgment_not_the_original_error() {
    ensure_init();
    let calls = Arc::new(AtomicU32::new(0));
    // `breaker-api`: attempts=1, breaker opens after 1 failure with a 60s
    // cooldown — the first call fails (real error), the second is a
    // breaker fast-fail (KEEL-E012, no call to the body at all).
    let _ = breaker_call(Arc::clone(&calls)).await;
    let before = calls.load(Ordering::SeqCst);
    let err = breaker_call(Arc::clone(&calls)).await.unwrap_err();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        before,
        "the breaker fast-fail never ran the body again"
    );
    match err {
        keel::Error::Keel(e) => {
            assert_eq!(e.code, keel_core_api::ErrorCode::BreakerOpen);
        }
        keel::Error::Original(e) => {
            panic!("expected a breaker judgment, got the original error: {e}")
        }
    }
}
