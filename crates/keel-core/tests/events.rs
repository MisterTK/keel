//! Engine ↔ live-event-sink integration: the NDJSON feed a later `keel tail`
//! follows, and the trace refs Tier 1 failure messages carry (dx-spec §6 +
//! invariant 4). Everything runs on tokio's paused clock with a fixed run id
//! ([`EventSink::to_writer`]), so the expected feeds — sequence numbers,
//! virtual-clock `ms` stamps and all — are asserted exactly.

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use keel_core_api::{AttemptResult, ENVELOPE_VERSION, ErrorClass, ErrorCode, Request};
use keelrun_core::Engine;
use keelrun_core::events::{CacheStore, Event, EventKind, EventSink, TraceRef};
use serde_json::json;

/// A `Write` the test keeps a handle on after the sink boxes it.
#[derive(Debug, Clone, Default)]
struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("buf lock").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl SharedBuf {
    /// Parse the feed written so far (call `flush()` on the sink first).
    fn events(&self) -> Vec<Event> {
        String::from_utf8(self.0.lock().expect("buf lock").clone())
            .expect("utf-8 feed")
            .lines()
            .map(|l| serde_json::from_str(l).expect("every feed line parses"))
            .collect()
    }
}

/// An engine with a deterministic sink attached under run id `run-test`.
fn wired_engine() -> (Engine, SharedBuf) {
    let buf = SharedBuf::default();
    let mut engine = Engine::new();
    engine.attach_events(
        EventSink::to_writer(Box::new(buf.clone()), "run-test").expect("sink must start"),
    );
    (engine, buf)
}

fn req(target: &str, args_hash: Option<&str>) -> Request {
    Request {
        v: ENVELOPE_VERSION,
        target: target.to_owned(),
        op: format!("GET {target}"),
        idempotent: true,
        args_hash: args_hash.map(str::to_owned),
    }
}

fn ev(seq: u64, ms: u64, kind: EventKind) -> Event {
    Event {
        v: 1,
        seq,
        ms,
        kind,
    }
}

fn run_start(seq: u64) -> Event {
    ev(
        seq,
        0,
        EventKind::RunStart {
            run: "run-test".to_owned(),
            wall_ms: None,
            pid: None,
        },
    )
}

/// Flush the engine's sink and return the parsed feed.
fn feed(engine: &Engine, buf: &SharedBuf) -> Vec<Event> {
    engine.events().expect("sink attached").flush();
    buf.events()
}

const TIMEOUT_ERR: fn() -> AttemptResult = || AttemptResult::Error {
    class: ErrorClass::Timeout,
    http_status: None,
    retry_after_ms: None,
    message: "read timeout".to_owned(),
    original: None,
};

#[expect(
    clippy::too_many_lines,
    reason = "the full expected feed is asserted literally — the length IS the test"
)]
#[tokio::test(start_paused = true)]
async fn retry_exhaustion_streams_ordered_events_with_virtual_timestamps() {
    let (engine, buf) = wired_engine();
    engine
        .configure(&json!({
            "target": { "api.slow.internal": {
                "retry": { "attempts": 3, "schedule": "exp(200ms, x2, max 30s)" }
            } }
        }))
        .expect("valid policy");

    let out = engine
        .execute(&req("api.slow.internal", None), async |_a| TIMEOUT_ERR())
        .await;

    let error = out.error.expect("terminal failure");
    assert_eq!(error.code, ErrorCode::AttemptsExhausted);
    // dx invariant 4: the failure message ends in a resolvable trace ref,
    // pointing at this call's `call_start` line (seq 1; the header owns 0).
    assert_eq!(
        error.message,
        "GET api.slow.internal failed 3/3 attempts (last: timeout). read timeout. \
         trace: keel trace run-test#1"
    );

    let call = || "t-000001".to_owned();
    let target = || "api.slow.internal".to_owned();
    assert_eq!(
        feed(&engine, &buf),
        vec![
            run_start(0),
            ev(
                1,
                0,
                EventKind::CallStart {
                    call: call(),
                    target: target(),
                    op: "GET api.slow.internal".to_owned(),
                }
            ),
            ev(
                2,
                0,
                EventKind::AttemptStart {
                    call: call(),
                    target: target(),
                    attempt: 1
                }
            ),
            ev(
                3,
                0,
                EventKind::AttemptError {
                    call: call(),
                    target: target(),
                    attempt: 1,
                    class: ErrorClass::Timeout,
                    http_status: None,
                }
            ),
            ev(
                4,
                0,
                EventKind::Backoff {
                    call: call(),
                    target: target(),
                    attempt: 1,
                    wait_ms: 200,
                }
            ),
            ev(
                5,
                200,
                EventKind::AttemptStart {
                    call: call(),
                    target: target(),
                    attempt: 2
                }
            ),
            ev(
                6,
                200,
                EventKind::AttemptError {
                    call: call(),
                    target: target(),
                    attempt: 2,
                    class: ErrorClass::Timeout,
                    http_status: None,
                }
            ),
            ev(
                7,
                200,
                EventKind::Backoff {
                    call: call(),
                    target: target(),
                    attempt: 2,
                    wait_ms: 400,
                }
            ),
            ev(
                8,
                600,
                EventKind::AttemptStart {
                    call: call(),
                    target: target(),
                    attempt: 3
                }
            ),
            ev(
                9,
                600,
                EventKind::AttemptError {
                    call: call(),
                    target: target(),
                    attempt: 3,
                    class: ErrorClass::Timeout,
                    http_status: None,
                }
            ),
            ev(
                10,
                600,
                EventKind::CallEnd {
                    call: call(),
                    target: target(),
                    result: "error".to_owned(),
                    code: Some(ErrorCode::AttemptsExhausted),
                    attempts: 3,
                }
            ),
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn a_trace_ref_resolves_back_to_exactly_its_calls_events() {
    let (engine, buf) = wired_engine();
    engine
        .configure(&json!({
            "target": { "api.slow.internal": { "retry": { "attempts": 2, "schedule": "exp(100ms, x2, max 1s)" } } }
        }))
        .expect("valid policy");

    // An unrelated successful call first, so resolution must actually filter.
    engine
        .execute(&req("api.other.internal", None), async |_a| {
            AttemptResult::Ok { payload: json!(1) }
        })
        .await;
    let out = engine
        .execute(&req("api.slow.internal", None), async |_a| TIMEOUT_ERR())
        .await;

    // Resolve the ref the way `keel trace` will: parse the token after
    // "trace: keel trace ", read the `call_start` line at `seq`, then select
    // the call's events by that line's `call` id.
    let message = out.error.expect("terminal failure").message;
    let token = message
        .split("trace: keel trace ")
        .nth(1)
        .expect("message carries a trace ref");
    let trace: TraceRef = token.parse().expect("ref parses");
    assert_eq!(trace.run, "run-test");
    assert_eq!(trace.file_name(), "run-test.ndjson");

    let events = feed(&engine, &buf);
    let anchor = events
        .iter()
        .find(|e| e.seq == trace.seq)
        .expect("anchor line exists");
    let EventKind::CallStart { call, target, .. } = &anchor.kind else {
        panic!("anchor must be call_start, got {anchor:?}");
    };
    assert_eq!(target, "api.slow.internal");
    let of_call: Vec<&Event> = events
        .iter()
        .filter(|e| match &e.kind {
            EventKind::CallStart { call: c, .. }
            | EventKind::CacheHit { call: c, .. }
            | EventKind::CacheMiss { call: c, .. }
            | EventKind::Throttle { call: c, .. }
            | EventKind::BreakerReject { call: c, .. }
            | EventKind::BreakerHalfOpen { call: c, .. }
            | EventKind::BreakerOpen { call: c, .. }
            | EventKind::BreakerClose { call: c, .. }
            | EventKind::AttemptStart { call: c, .. }
            | EventKind::AttemptError { call: c, .. }
            | EventKind::Backoff { call: c, .. }
            | EventKind::CallEnd { call: c, .. } => c == call,
            EventKind::RunStart { .. } => false,
        })
        .collect();
    // call_start, 2× (attempt_start + attempt_error), backoff, call_end.
    assert_eq!(of_call.len(), 7);
    assert!(matches!(of_call[0].kind, EventKind::CallStart { .. }));
    assert!(matches!(
        of_call[6].kind,
        EventKind::CallEnd {
            code: Some(ErrorCode::AttemptsExhausted),
            ..
        }
    ));
    // The unrelated call's events are excluded.
    assert!(of_call.iter().all(|e| e.seq >= trace.seq));
}

#[expect(
    clippy::too_many_lines,
    reason = "the full expected feed is asserted literally — the length IS the test"
)]
#[tokio::test(start_paused = true)]
async fn breaker_lifecycle_streams_open_reject_halfopen_close() {
    let (engine, buf) = wired_engine();
    engine
        .configure(&json!({
            "target": { "api.flaky.internal": {
                "retry": { "attempts": 1 },
                "breaker": { "failures": 1, "cooldown": "15s" }
            } }
        }))
        .expect("valid policy");
    let request = req("api.flaky.internal", None);
    let call = |n: &str| n.to_owned();
    let target = || "api.flaky.internal".to_owned();

    // Call 1 fails terminally and trips the breaker (failures = 1).
    let out = engine
        .execute(&request, async |_a| AttemptResult::Error {
            class: ErrorClass::Conn,
            http_status: None,
            retry_after_ms: None,
            message: "connection refused".to_owned(),
            original: None,
        })
        .await;
    assert_eq!(
        out.error.expect("terminal").code,
        ErrorCode::AttemptsExhausted
    );

    // Call 2 is rejected fast — and its message carries its own trace ref.
    let out = engine
        .execute(&request, async |_a| unreachable!("breaker is open"))
        .await;
    let error = out.error.expect("rejected");
    assert_eq!(error.code, ErrorCode::BreakerOpen);
    assert_eq!(
        error.message,
        "breaker OPEN for api.flaky.internal: failed fast, call not attempted. \
         trace: keel trace run-test#6"
    );

    // Cooldown elapses; call 3 is the half-open probe and closes the breaker.
    tokio::time::advance(Duration::from_secs(15)).await;
    let out = engine
        .execute(&request, async |_a| AttemptResult::Ok { payload: json!(1) })
        .await;
    assert_eq!(out.result, "ok");

    assert_eq!(
        feed(&engine, &buf),
        vec![
            run_start(0),
            ev(
                1,
                0,
                EventKind::CallStart {
                    call: call("t-000001"),
                    target: target(),
                    op: "GET api.flaky.internal".to_owned(),
                }
            ),
            ev(
                2,
                0,
                EventKind::AttemptStart {
                    call: call("t-000001"),
                    target: target(),
                    attempt: 1,
                }
            ),
            ev(
                3,
                0,
                EventKind::AttemptError {
                    call: call("t-000001"),
                    target: target(),
                    attempt: 1,
                    class: ErrorClass::Conn,
                    http_status: None,
                }
            ),
            ev(
                4,
                0,
                EventKind::BreakerOpen {
                    call: call("t-000001"),
                    target: target(),
                    cooldown_ms: 15_000,
                }
            ),
            ev(
                5,
                0,
                EventKind::CallEnd {
                    call: call("t-000001"),
                    target: target(),
                    result: "error".to_owned(),
                    code: Some(ErrorCode::AttemptsExhausted),
                    attempts: 1,
                }
            ),
            ev(
                6,
                0,
                EventKind::CallStart {
                    call: call("t-000002"),
                    target: target(),
                    op: "GET api.flaky.internal".to_owned(),
                }
            ),
            ev(
                7,
                0,
                EventKind::BreakerReject {
                    call: call("t-000002"),
                    target: target(),
                }
            ),
            ev(
                8,
                0,
                EventKind::CallEnd {
                    call: call("t-000002"),
                    target: target(),
                    result: "error".to_owned(),
                    code: Some(ErrorCode::BreakerOpen),
                    attempts: 0,
                }
            ),
            ev(
                9,
                15_000,
                EventKind::CallStart {
                    call: call("t-000003"),
                    target: target(),
                    op: "GET api.flaky.internal".to_owned(),
                }
            ),
            ev(
                10,
                15_000,
                EventKind::BreakerHalfOpen {
                    call: call("t-000003"),
                    target: target(),
                }
            ),
            ev(
                11,
                15_000,
                EventKind::AttemptStart {
                    call: call("t-000003"),
                    target: target(),
                    attempt: 1,
                }
            ),
            ev(
                12,
                15_000,
                EventKind::BreakerClose {
                    call: call("t-000003"),
                    target: target(),
                }
            ),
            ev(
                13,
                15_000,
                EventKind::CallEnd {
                    call: call("t-000003"),
                    target: target(),
                    result: "ok".to_owned(),
                    code: None,
                    attempts: 1,
                }
            ),
        ]
    );
}

#[expect(
    clippy::too_many_lines,
    reason = "the full expected feed is asserted literally — the length IS the test"
)]
#[tokio::test(start_paused = true)]
async fn cache_and_rate_layers_stream_miss_hit_and_throttle() {
    let (engine, buf) = wired_engine();
    engine
        .configure(&json!({
            "target": { "api.catalog.internal": { "cache": { "ttl": "60s" }, "rate": "1/s" } }
        }))
        .expect("valid policy");
    let target = || "api.catalog.internal".to_owned();
    let op = || "GET api.catalog.internal".to_owned();

    // k1 live (miss), k1 again (hit, no rate consumed), k2 live (throttled).
    for args in ["k1", "k1", "k2"] {
        let out = engine
            .execute(&req("api.catalog.internal", Some(args)), async |_a| {
                AttemptResult::Ok { payload: json!(1) }
            })
            .await;
        assert_eq!(out.result, "ok", "{args}");
    }

    assert_eq!(
        feed(&engine, &buf),
        vec![
            run_start(0),
            ev(
                1,
                0,
                EventKind::CallStart {
                    call: "t-000001".to_owned(),
                    target: target(),
                    op: op(),
                }
            ),
            ev(
                2,
                0,
                EventKind::CacheMiss {
                    call: "t-000001".to_owned(),
                    target: target(),
                    scope: CacheStore::Memory,
                }
            ),
            ev(
                3,
                0,
                EventKind::AttemptStart {
                    call: "t-000001".to_owned(),
                    target: target(),
                    attempt: 1,
                }
            ),
            ev(
                4,
                0,
                EventKind::CallEnd {
                    call: "t-000001".to_owned(),
                    target: target(),
                    result: "ok".to_owned(),
                    code: None,
                    attempts: 1,
                }
            ),
            ev(
                5,
                0,
                EventKind::CallStart {
                    call: "t-000002".to_owned(),
                    target: target(),
                    op: op(),
                }
            ),
            ev(
                6,
                0,
                EventKind::CacheHit {
                    call: "t-000002".to_owned(),
                    target: target(),
                    scope: CacheStore::Memory,
                }
            ),
            ev(
                7,
                0,
                EventKind::CallEnd {
                    call: "t-000002".to_owned(),
                    target: target(),
                    result: "ok".to_owned(),
                    code: None,
                    attempts: 0,
                }
            ),
            ev(
                8,
                0,
                EventKind::CallStart {
                    call: "t-000003".to_owned(),
                    target: target(),
                    op: op(),
                }
            ),
            ev(
                9,
                0,
                EventKind::CacheMiss {
                    call: "t-000003".to_owned(),
                    target: target(),
                    scope: CacheStore::Memory,
                }
            ),
            ev(
                10,
                0,
                EventKind::Throttle {
                    call: "t-000003".to_owned(),
                    target: target(),
                    wait_ms: 1000,
                }
            ),
            ev(
                11,
                1000,
                EventKind::AttemptStart {
                    call: "t-000003".to_owned(),
                    target: target(),
                    attempt: 1,
                }
            ),
            ev(
                12,
                1000,
                EventKind::CallEnd {
                    call: "t-000003".to_owned(),
                    target: target(),
                    result: "ok".to_owned(),
                    code: None,
                    attempts: 1,
                }
            ),
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn envelope_version_failures_still_open_and_close_the_call_in_the_feed() {
    let (engine, buf) = wired_engine();
    let mut request = req("api.example.com", None);
    request.v = 999;

    let out = engine
        .execute(&request, async |_a| unreachable!("never attempted"))
        .await;
    assert_eq!(
        out.error.expect("version error").code,
        ErrorCode::EnvelopeVersion
    );

    let events = feed(&engine, &buf);
    assert_eq!(
        events.len(),
        3,
        "run_start, call_start, call_end: {events:?}"
    );
    assert!(matches!(events[1].kind, EventKind::CallStart { .. }));
    assert!(matches!(
        events[2].kind,
        EventKind::CallEnd {
            code: Some(ErrorCode::EnvelopeVersion),
            attempts: 0,
            ..
        }
    ));
}

/// Without a sink — the conformance condition — failure messages carry no
/// trace ref, byte-identical to the stubs (parity rule).
#[tokio::test(start_paused = true)]
async fn without_a_sink_messages_carry_no_trace_ref() {
    let engine = Engine::new();
    assert!(
        engine.events().is_none(),
        "test env must not activate events (KEEL_EVENTS unset, no ./.keel)"
    );
    engine
        .configure(&json!({
            "target": { "api.slow.internal": { "retry": { "attempts": 2, "schedule": "exp(200ms, x2, max 30s)" } } }
        }))
        .expect("valid policy");

    let out = engine
        .execute(&req("api.slow.internal", None), async |_a| TIMEOUT_ERR())
        .await;
    assert_eq!(
        out.error.expect("terminal failure").message,
        "GET api.slow.internal failed 2/2 attempts (last: timeout). read timeout"
    );
}
