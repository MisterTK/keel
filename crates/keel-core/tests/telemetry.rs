//! Telemetry: the `keel.call` / `keel.attempt` span hierarchy and its fields
//! (architecture spec §4.5). These assert *structure*, not outcomes — the
//! conformance corpus already pins behavior, and instrumentation must not touch
//! it. A hand-rolled capturing `Subscriber` (no `tracing-subscriber` dependency,
//! mirroring `tests/journal.rs`'s `WarnCounter`) records span names, contextual
//! parents, and fields so the three load-bearing shapes can be checked:
//! retry-then-success (1 call, 2 attempt children), a cache hit (1 call, 0
//! attempts, `from_cache=true`), and breaker-open fast-fail (1 call, 0 attempts,
//! `breaker=open`).

use core::fmt;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use keel_core_api::{AttemptResult, ENVELOPE_VERSION, ErrorClass, Request};
use keelrun_core::Engine;
use serde_json::json;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

fn request(target: &str, idempotent: bool) -> Request {
    Request {
        v: ENVELOPE_VERSION,
        target: target.to_owned(),
        op: format!("GET {target}/x"),
        idempotent,
        args_hash: None,
    }
}

fn cache_request(target: &str) -> Request {
    Request {
        args_hash: Some("args-1".to_owned()),
        ..request(target, true)
    }
}

/// One recorded span: its name, contextual parent (if any), and the merged set
/// of fields (declared at open plus any recorded afterwards).
#[derive(Debug)]
struct SpanRecord {
    name: &'static str,
    parent: Option<u64>,
    fields: BTreeMap<String, String>,
}

/// Everything the capturing subscriber has seen so far, guarded by one mutex.
#[derive(Debug, Default)]
struct Captured {
    next_id: u64,
    /// Currently-entered span ids (single-threaded under `start_paused`, so a
    /// plain stack tracks the contextual parent correctly).
    stack: Vec<u64>,
    spans: BTreeMap<u64, SpanRecord>,
    events: Vec<BTreeMap<String, String>>,
}

impl Captured {
    /// The single span with `name`; panics unless exactly one exists.
    fn only(&self, name: &str) -> &SpanRecord {
        let mut matches = self.spans.values().filter(|record| record.name == name);
        let found = matches.next().expect("expected a span with this name");
        assert!(matches.next().is_none(), "expected exactly one {name} span");
        found
    }

    /// Every span named `name` whose contextual parent is `parent`.
    fn children(&self, parent: u64, name: &str) -> Vec<&SpanRecord> {
        self.spans
            .values()
            .filter(|record| record.name == name && record.parent == Some(parent))
            .collect()
    }

    fn id_of(&self, record: &SpanRecord) -> u64 {
        *self
            .spans
            .iter()
            .find(|(_, candidate)| std::ptr::eq(*candidate, record))
            .expect("record belongs to this capture")
            .0
    }

    fn has_event(&self, key: &str, value: &str) -> bool {
        self.events
            .iter()
            .any(|fields| fields.get(key).is_some_and(|found| found == value))
    }
}

/// A `tracing` subscriber that records the span tree into a shared [`Captured`].
#[derive(Clone, Debug, Default)]
struct Capture(Arc<Mutex<Captured>>);

impl Capture {
    fn captured(&self) -> std::sync::MutexGuard<'_, Captured> {
        self.0.lock().expect("capture lock poisoned")
    }
}

impl Subscriber for Capture {
    fn register_callsite(
        &self,
        _metadata: &'static Metadata<'static>,
    ) -> tracing::subscriber::Interest {
        tracing::subscriber::Interest::always()
    }

    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, attrs: &Attributes<'_>) -> Id {
        let mut captured = self.captured();
        captured.next_id += 1;
        let id = captured.next_id;
        let parent = if attrs.is_root() {
            None
        } else if let Some(explicit) = attrs.parent() {
            Some(explicit.into_u64())
        } else {
            captured.stack.last().copied()
        };
        let mut fields = BTreeMap::new();
        attrs.record(&mut FieldVisitor(&mut fields));
        captured.spans.insert(
            id,
            SpanRecord {
                name: attrs.metadata().name(),
                parent,
                fields,
            },
        );
        Id::from_u64(id)
    }

    fn record(&self, span: &Id, values: &Record<'_>) {
        let mut captured = self.captured();
        if let Some(record) = captured.spans.get_mut(&span.into_u64()) {
            values.record(&mut FieldVisitor(&mut record.fields));
        }
    }

    fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut fields = BTreeMap::new();
        event.record(&mut FieldVisitor(&mut fields));
        self.captured().events.push(fields);
    }

    fn enter(&self, span: &Id) {
        self.captured().stack.push(span.into_u64());
    }

    fn exit(&self, span: &Id) {
        let mut captured = self.captured();
        if let Some(pos) = captured.stack.iter().rposition(|id| *id == span.into_u64()) {
            captured.stack.remove(pos);
        }
    }
}

/// Flattens any recorded field into a string; `%`/`?` values arrive as
/// `record_debug`, so `Display`-typed fields (target, op, event messages) land
/// as their plain rendered text.
#[derive(Debug)]
struct FieldVisitor<'a>(&'a mut BTreeMap<String, String>);

impl Visit for FieldVisitor<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.0.insert(field.name().to_owned(), format!("{value:?}"));
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_owned(), value.to_owned());
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.0.insert(field.name().to_owned(), value.to_string());
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.0.insert(field.name().to_owned(), value.to_string());
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.0.insert(field.name().to_owned(), value.to_string());
    }
}

fn field<'a>(record: &'a SpanRecord, key: &str) -> &'a str {
    record
        .fields
        .get(key)
        .unwrap_or_else(|| panic!("span {} missing field {key}", record.name))
}

/// A retry-then-success call opens one `keel.call` span with two `keel.attempt`
/// children; the call's terminal fields reflect the outcome and each attempt
/// carries its number, class/status, and (for the retried attempt) the wait.
#[tokio::test(start_paused = true)]
async fn retry_then_success_opens_one_call_and_two_attempt_children() {
    let capture = Capture::default();
    let guard = tracing::dispatcher::set_default(&tracing::Dispatch::new(capture.clone()));

    let engine = Engine::new();
    engine
        .configure(&json!({
            "target": { "api.x": {
                "retry": { "attempts": 3, "schedule": "fixed(10ms)", "on": ["conn"] }
            } }
        }))
        .expect("valid policy");

    let out = engine
        .execute(&request("api.x", true), async |attempt| {
            if attempt < 2 {
                AttemptResult::Error {
                    class: ErrorClass::Conn,
                    http_status: None,
                    retry_after_ms: None,
                    message: String::from("refused"),
                    original: None,
                }
            } else {
                AttemptResult::Ok {
                    payload: json!("ok"),
                }
            }
        })
        .await;

    drop(guard);
    assert_eq!(out.result, "ok");
    assert_eq!(out.attempts, 2);

    let captured = capture.captured();
    let call = captured.only("keel.call");
    assert_eq!(field(call, "target"), "api.x");
    assert_eq!(field(call, "result"), "ok");
    assert_eq!(field(call, "attempts"), "2");
    assert_eq!(field(call, "from_cache"), "false");
    assert_eq!(field(call, "throttled"), "false");
    assert_eq!(field(call, "breaker"), "closed");
    assert_eq!(field(call, "trace_id"), out.trace_id);

    let call_id = captured.id_of(call);
    let mut attempts = captured.children(call_id, "keel.attempt");
    assert_eq!(
        attempts.len(),
        2,
        "retry-then-success = two attempt children"
    );
    attempts.sort_by_key(|record| field(record, "attempt").to_owned());

    // Attempt 1: the conn failure that was retried, with a recorded wait.
    assert_eq!(field(attempts[0], "attempt"), "1");
    assert_eq!(field(attempts[0], "result"), "error");
    assert_eq!(field(attempts[0], "class"), "conn");
    assert_eq!(field(attempts[0], "wait_ms"), "10");
    // Attempt 2: the success (no wait recorded).
    assert_eq!(field(attempts[1], "attempt"), "2");
    assert_eq!(field(attempts[1], "result"), "ok");
    assert!(!attempts[1].fields.contains_key("wait_ms"));
}

/// A cache hit opens a `keel.call` span with `from_cache=true` and zero attempt
/// children (the effect never runs), and emits a debug cache-hit event.
#[tokio::test(start_paused = true)]
async fn cache_hit_opens_call_span_with_no_attempts() {
    let engine = Engine::new();
    engine
        .configure(&json!({ "target": { "api.cached": { "cache": { "ttl": "5m" } } } }))
        .expect("valid policy");
    let req = cache_request("api.cached");

    // Populate the cache off-subscriber, so the capture sees only the hit.
    let cold = engine
        .execute(&req, async |_| AttemptResult::Ok {
            payload: json!("v1"),
        })
        .await;
    assert!(!cold.from_cache);
    assert_eq!(cold.attempts, 1);

    let capture = Capture::default();
    let guard = tracing::dispatcher::set_default(&tracing::Dispatch::new(capture.clone()));
    let hit = engine
        .execute(&req, async |_| AttemptResult::Ok {
            payload: json!("SENTINEL-must-not-run"),
        })
        .await;
    drop(guard);

    assert!(hit.from_cache);
    assert_eq!(hit.attempts, 0);
    assert_eq!(hit.payload, Some(json!("v1")));

    let captured = capture.captured();
    let call = captured.only("keel.call");
    assert_eq!(field(call, "from_cache"), "true");
    assert_eq!(field(call, "result"), "ok");
    let call_id = captured.id_of(call);
    assert!(
        captured.children(call_id, "keel.attempt").is_empty(),
        "a cache hit runs no attempts"
    );
    assert!(
        captured.has_event("message", "cache hit"),
        "the cache hit emits a debug event"
    );
}

/// A breaker-open fast-fail opens a `keel.call` span with `breaker=open`, the
/// KEEL-E012 error code, and zero attempt children; tripping it earlier emits a
/// breaker-transition debug event.
#[tokio::test(start_paused = true)]
async fn breaker_open_fast_fail_opens_call_span_with_no_attempts() {
    let engine = Engine::new();
    engine
        .configure(&json!({
            "target": { "api.down": { "breaker": { "failures": 1, "cooldown": "60s" } } }
        }))
        .expect("valid policy");
    let req = request("api.down", true);

    let capture = Capture::default();
    let guard = tracing::dispatcher::set_default(&tracing::Dispatch::new(capture.clone()));

    // One terminal failure trips the breaker (failures = 1).
    let tripped = engine
        .execute(&req, async |_| AttemptResult::Error {
            class: ErrorClass::Conn,
            http_status: None,
            retry_after_ms: None,
            message: String::from("boom"),
            original: None,
        })
        .await;
    assert_eq!(tripped.result, "error");

    // The next call fails fast: breaker OPEN, effect never invoked.
    let fast = engine
        .execute(&req, async |_| AttemptResult::Ok {
            payload: json!("SENTINEL-must-not-run"),
        })
        .await;
    drop(guard);

    assert_eq!(fast.result, "error");
    assert_eq!(fast.attempts, 0);
    assert_eq!(
        fast.error.as_ref().expect("terminal error").code.as_str(),
        "KEEL-E012"
    );

    let captured = capture.captured();
    let calls: Vec<&SpanRecord> = captured
        .spans
        .values()
        .filter(|record| record.name == "keel.call")
        .collect();
    assert_eq!(calls.len(), 2, "two calls were made");

    let fast_call = calls
        .iter()
        .find(|record| {
            record
                .fields
                .get("error_code")
                .is_some_and(|code| code == "KEEL-E012")
        })
        .expect("the fast-fail call span");
    assert_eq!(field(fast_call, "breaker"), "open");
    let fast_id = captured.id_of(fast_call);
    assert!(
        captured.children(fast_id, "keel.attempt").is_empty(),
        "a fast-fail runs no attempts"
    );
    assert!(
        captured.has_event("transition", "opened"),
        "tripping the breaker emits a transition event"
    );
}
