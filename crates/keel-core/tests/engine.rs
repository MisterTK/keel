//! Engine behaviors beyond the shared conformance corpus: the enforced
//! policy timeout layer (KEEL-E011) and schedule jitter bounds.

use std::time::Duration;

use keel_core_api::{AttemptResult, ENVELOPE_VERSION, ErrorClass, Request};
use keelrun_core::Engine;
use serde_json::json;

fn request(target: &str, idempotent: bool) -> Request {
    Request {
        v: ENVELOPE_VERSION,
        target: target.to_owned(),
        op: format!("GET {target}/x"),
        idempotent,
        args_hash: None,
    }
}

/// A hung call is cut off by the policy timeout and diagnosed as KEEL-E011
/// with class `timeout`, not as a generic retry exhaustion.
#[tokio::test(start_paused = true)]
async fn policy_timeout_terminates_with_e011() {
    let engine = Engine::new();
    engine
        .configure(&json!({
            "target": { "api.hung.internal": {
                "timeout": "50ms",
                "retry": { "attempts": 1 }
            } }
        }))
        .expect("valid policy");

    let outcome = engine
        .execute(&request("api.hung.internal", true), async |_attempt| {
            tokio::time::sleep(Duration::from_hours(1)).await;
            AttemptResult::Ok {
                payload: json!("too late"),
            }
        })
        .await;

    assert_eq!(outcome.result, "error");
    assert_eq!(outcome.attempts, 1);
    let error = outcome.error.expect("terminal error");
    assert_eq!(error.code.as_str(), "KEEL-E011");
    assert_eq!(error.class, ErrorClass::Timeout);
}

/// Layer timeouts are retried like any timeout-class error; only the final
/// failure is diagnosed as the timeout (retries happened, waits recorded).
#[tokio::test(start_paused = true)]
async fn policy_timeout_is_retried_then_e011() {
    let engine = Engine::new();
    engine
        .configure(&json!({
            "target": { "api.hung.internal": {
                "timeout": "50ms",
                "retry": { "attempts": 3, "schedule": "exp(100ms, x2, max 1s)", "on": ["timeout"] }
            } }
        }))
        .expect("valid policy");

    let outcome = engine
        .execute(&request("api.hung.internal", true), async |_attempt| {
            tokio::time::sleep(Duration::from_hours(1)).await;
            AttemptResult::Ok {
                payload: json!("too late"),
            }
        })
        .await;

    assert_eq!(outcome.attempts, 3);
    assert_eq!(outcome.waits_ms, vec![100, 200]);
    assert_eq!(
        outcome.error.expect("terminal error").code.as_str(),
        "KEEL-E011"
    );
}

/// Level 0 hard rule: the policy per-attempt timeout is NEVER armed on a
/// non-idempotent call. Firing it would drop the in-flight effect while the
/// POST may still commit server-side and hand back a synthetic timeout for a
/// call that actually succeeded. So a slow-but-succeeding non-idempotent call
/// runs to completion rather than being cut off (mirrors the front ends'
/// judgment; the core must not defeat their guard).
#[tokio::test(start_paused = true)]
async fn non_idempotent_call_is_never_cut_off_by_policy_timeout() {
    let engine = Engine::new();
    engine
        .configure(&json!({
            "target": { "api.hung.internal": {
                "timeout": "50ms",
                "retry": { "attempts": 3, "on": ["timeout"] }
            } }
        }))
        .expect("valid policy");

    let outcome = engine
        .execute(&request("api.hung.internal", false), async |_attempt| {
            // Far longer than the 50ms policy timeout: if the timeout were
            // (wrongly) armed here it would fire and drop this future.
            tokio::time::sleep(Duration::from_hours(1)).await;
            AttemptResult::Ok {
                payload: json!("committed"),
            }
        })
        .await;

    assert_eq!(
        outcome.result, "ok",
        "non-idempotent success is not cut off"
    );
    assert_eq!(outcome.attempts, 1);
    assert_eq!(outcome.payload, Some(json!("committed")));
    assert!(outcome.error.is_none());
}

/// An idempotent call, by contrast, IS protected by the policy timeout: a hung
/// GET is cut off at the deadline (the guard above is idempotency-gated, not a
/// blanket disable).
#[tokio::test(start_paused = true)]
async fn idempotent_call_still_honors_policy_timeout() {
    let engine = Engine::new();
    engine
        .configure(&json!({
            "target": { "api.hung.internal": {
                "timeout": "50ms",
                "retry": { "attempts": 1 }
            } }
        }))
        .expect("valid policy");

    let outcome = engine
        .execute(&request("api.hung.internal", true), async |_attempt| {
            tokio::time::sleep(Duration::from_hours(1)).await;
            AttemptResult::Ok {
                payload: json!("too late"),
            }
        })
        .await;

    assert_eq!(outcome.result, "error");
    assert_eq!(
        outcome.error.expect("terminal error").code.as_str(),
        "KEEL-E011"
    );
}

/// Equal jitter: every wait is uniform in [w/2, w] of the deterministic
/// schedule value, and Retry-After still overrides upward.
#[tokio::test(start_paused = true)]
async fn jitter_stays_within_equal_jitter_bounds() {
    fastrand::seed(7);
    let engine = Engine::new();
    engine
        .configure(&json!({
            "target": { "api.flaky.internal": {
                "retry": { "attempts": 5, "schedule": "exp(1s, x2, max 30s, jitter)", "on": ["conn"] }
            } }
        }))
        .expect("valid policy");

    let outcome = engine
        .execute(&request("api.flaky.internal", true), async |_attempt| {
            AttemptResult::Error {
                class: ErrorClass::Conn,
                http_status: None,
                retry_after_ms: None,
                message: String::from("refused"),
                original: None,
            }
        })
        .await;

    assert_eq!(outcome.attempts, 5);
    assert_eq!(outcome.waits_ms.len(), 4);
    for (i, (&wait, expected)) in outcome
        .waits_ms
        .iter()
        .zip([1_000u64, 2_000, 4_000, 8_000])
        .enumerate()
    {
        assert!(
            (expected / 2..=expected).contains(&wait),
            "wait[{i}] = {wait} outside [{}, {expected}]",
            expected / 2
        );
    }
}

/// CCR-11 coverage gap (#128 follow-up): `until.absent = "pending"` still
/// terminates at the poll deadline like any other pending verdict — the
/// deadline check in `run_poll` is on `PollVerdict::Pending` regardless of
/// whether that verdict came from the field being present-but-not-terminal
/// or from `absent` mapping a MISSING field to pending. This matters because
/// CCR-11 deliberately allows `absent = "pending"` on a host-level table,
/// where a wrong path would otherwise poll forever; the deadline is the only
/// thing bounding it. Both stubs already pin this (`test_stub_poll_v2.py`'s
/// `test_absent_pending_still_reaches_the_deadline`, Node's `poll-v2.test.mjs`
/// "until.absent = pending still honors the deadline") — the real core did
/// not.
#[tokio::test(start_paused = true)]
async fn poll_absent_pending_still_hits_the_deadline() {
    let engine = Engine::new();
    engine
        .configure(&json!({
            "target": { "lro.example": {
                "poll": {
                    "interval": "10s",
                    "deadline": "25s",
                    "until": { "field": "done", "terminal": [true], "absent": "pending" }
                }
            } }
        }))
        .expect("valid policy");

    // Every attempt is a running google.longrunning.Operation body: `done`
    // is never present, only `name`. Under `absent = "pending"` this must
    // keep polling — never fail open — until the deadline is exceeded.
    let outcome = engine
        .execute(&request("lro.example", true), async |_attempt| {
            AttemptResult::Ok {
                payload: json!({ "name": "projects/p/operations/op1" }),
            }
        })
        .await;

    assert_eq!(outcome.result, "error");
    let error = outcome.error.expect("terminal error");
    assert_eq!(error.code.as_str(), "KEEL-E016");
    assert_eq!(
        error.message,
        "GET lro.example/x poll deadline exceeded: 'done' not terminal after 25000ms"
    );
}
