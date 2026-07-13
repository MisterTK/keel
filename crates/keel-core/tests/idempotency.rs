//! Idempotency-key injection, core-side (contracts/adapter-pack.md
//! "Idempotency-key injection"): the resolved `idempotency` layer surfaces
//! through the engine, and a key the adapter minted for a Tier 2 step is
//! journaled in the step's `running` record so a crashed step's re-execution
//! reuses the SAME key (rule 3 — the at-least-once honesty story).
//!
//! Injection itself (minting, header placement, the judgment flip) is the
//! adapters' business and is tested in the front ends; here we pin the core
//! surfaces they rely on.

use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};

use keel_core::{Engine, FlowDescriptor, FlowManager};
use keel_core_api::{AttemptResult, ENVELOPE_VERSION, Request};
use keel_journal::{Clock, Journal, ManualClock, ProcessId, SqliteJournal, StepStatus};
use serde_json::{Value, json};
use tempfile::TempDir;

const T0: i64 = 1_783_728_000_000;

fn request(target: &str, args_hash: &str) -> Request {
    Request {
        v: ENVELOPE_VERSION,
        target: target.to_owned(),
        op: format!("POST {target}/charges"),
        idempotent: true, // the adapter injected a key, flipping the judgment
        args_hash: Some(args_hash.to_owned()),
    }
}

// --- engine surface ----------------------------------------------------------

#[test]
fn engine_resolves_the_idempotency_header_per_target() {
    let engine = Engine::new();
    engine
        .configure(&json!({
            "defaults": { "outbound": { "idempotency": { "header": "X-Idem" } } },
            "target": {
                "api.stripe.com": { "idempotency": { "header": "Idempotency-Key" } },
                "api.nokey.example": { "timeout": "1s" }
            }
        }))
        .expect("valid policy");

    // Exact target entry wins; other targets fall through to defaults.outbound.
    assert_eq!(
        engine.idempotency_header("api.stripe.com").as_deref(),
        Some("Idempotency-Key")
    );
    assert_eq!(
        engine.idempotency_header("api.nokey.example").as_deref(),
        Some("X-Idem")
    );

    // Read live: a reconfigure without the knob resolves to None again.
    engine.configure(&json!({})).expect("valid policy");
    assert_eq!(engine.idempotency_header("api.stripe.com"), None);
}

// --- Tier 2: the key rides the `running` record ------------------------------

struct Fixture {
    _dir: TempDir,
    clock: ManualClock,
    journal: Arc<dyn Journal>,
    manager: FlowManager,
    desc: FlowDescriptor,
}

fn fixture() -> Fixture {
    let dir = TempDir::new().expect("tempdir");
    let clock = ManualClock::new(T0);
    let journal: Arc<dyn Journal> =
        Arc::new(SqliteJournal::open(dir.path().join("journal.db"), clock.clone()).expect("open"));
    let engine = Engine::new();
    engine
        .configure(&json!({
            "flows": { "on_nondeterminism": "fail" },
            "target": { "api.pay.example": { "idempotency": { "header": "Idempotency-Key" } } }
        }))
        .expect("valid policy");
    let clock_dyn: Arc<dyn Clock> = Arc::new(clock.clone());
    let manager = FlowManager::new(
        Arc::new(engine),
        Arc::clone(&journal),
        clock_dyn,
        ProcessId::new("host-idem:pid-1"),
    );
    let desc = FlowDescriptor {
        entrypoint: String::from("py:billing.run:main"),
        args_hash: String::from("ah-idem"),
        explicit_key: None,
        code_hash: None,
    };
    Fixture {
        _dir: dir,
        clock,
        journal,
        manager,
        desc,
    }
}

/// Decode a schema-tagged step payload (`keel.step/v1`) to its inner value.
fn payload_value(bytes: &[u8]) -> Value {
    let envelope: Value = rmp_serde::from_slice(bytes).expect("payload decodes");
    assert_eq!(envelope["schema"], "keel.step/v1", "schema-tagged payload");
    envelope["payload"].clone()
}

#[tokio::test(start_paused = true)]
async fn crashed_running_step_journals_and_resurfaces_its_key() {
    let fx = fixture();

    // Run 1: step 1 completes with key ik-1; step 2 is interrupted mid-effect
    // (kill -9 shape) after its `running` record — carrying ik-2 — is journaled.
    {
        let mut handle = fx.manager.enter_flow(&fx.desc).expect("enter");
        assert_eq!(
            handle.recorded_idempotency_key("api.pay.example#c1"),
            None,
            "a fresh step has no recorded key"
        );
        let out = handle
            .execute_step_with_idempotency_key(
                &request("api.pay.example", "c1"),
                Some("ik-1"),
                |_attempt: u32| async {
                    AttemptResult::Ok {
                        payload: json!({ "charge": "ch_1" }),
                    }
                },
            )
            .await;
        assert_eq!(out.result, "ok");

        {
            let fut = handle.execute_step_with_idempotency_key(
                &request("api.pay.example", "c2"),
                Some("ik-2"),
                |_attempt: u32| std::future::pending::<AttemptResult>(),
            );
            let mut fut = pin!(fut);
            let mut cx = Context::from_waker(Waker::noop());
            assert!(
                matches!(fut.as_mut().poll(&mut cx), Poll::Pending),
                "the hung step must be in flight, not resolved"
            );
            // Dropping the in-flight future models the crash mid-step.
        }
        drop(handle); // crash: flow stays running with its lease
    }
    fx.clock.advance(31_000); // past the default 30s lease

    // The `running` record carries the key in its schema-tagged payload.
    let (key, running) = fx
        .journal
        .step_at(&fx.desc.flow_id(), 2)
        .expect("read")
        .expect("step 2 recorded");
    assert_eq!(key.as_str(), "api.pay.example#c2");
    assert_eq!(running.status, StepStatus::Running);
    let payload = payload_value(running.payload.as_deref().expect("running payload"));
    assert_eq!(payload, json!({ "idempotency_key": "ik-2" }));

    // Run 2 (resume): step 1 is substituted; step 2's recorded key is read back
    // so the re-execution injects the SAME key (provider-side dedup).
    let mut handle = fx.manager.enter_flow(&fx.desc).expect("resume");
    let effect_calls = Arc::new(AtomicUsize::new(0));
    let calls = Arc::clone(&effect_calls);
    let out = handle
        .execute_step_with_idempotency_key(
            &request("api.pay.example", "c1"),
            Some("ik-unused"),
            move |_attempt: u32| {
                let calls = Arc::clone(&calls);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    AttemptResult::Ok {
                        payload: json!({ "charge": "never" }),
                    }
                }
            },
        )
        .await;
    assert_eq!(out.result, "ok");
    assert_eq!(out.payload, Some(json!({ "charge": "ch_1" })));
    assert_eq!(
        effect_calls.load(Ordering::SeqCst),
        0,
        "a substituted step must not fire its effect"
    );

    // The peek: same step key + `running` record => the recorded key. A
    // different step key must NOT surface it (the key belongs to that step).
    assert_eq!(handle.recorded_idempotency_key("api.pay.example#other"), None);
    assert_eq!(
        handle.recorded_idempotency_key("api.pay.example#c2").as_deref(),
        Some("ik-2")
    );

    let out = handle
        .execute_step_with_idempotency_key(
            &request("api.pay.example", "c2"),
            Some("ik-2"), // what the adapter injects, having read the peek
            |_attempt: u32| async {
                AttemptResult::Ok {
                    payload: json!({ "charge": "ch_2" }),
                }
            },
        )
        .await;
    assert_eq!(out.result, "ok");
    assert_eq!(out.attempts, 1);
    handle.complete_success();

    // The step is now terminal; its payload is the outcome, and the peek no
    // longer surfaces a key (a terminal step is substituted, never re-sent).
    let (_, terminal) = fx
        .journal
        .step_at(&fx.desc.flow_id(), 2)
        .expect("read")
        .expect("step 2 recorded");
    assert_eq!(terminal.status, StepStatus::Ok);
    assert_eq!(
        payload_value(terminal.payload.as_deref().expect("terminal payload")),
        json!({ "charge": "ch_2" })
    );
}

#[tokio::test(start_paused = true)]
async fn steps_without_a_key_journal_no_payload_while_running() {
    let fx = fixture();
    let mut handle = fx.manager.enter_flow(&fx.desc).expect("enter");

    // `execute_step` (no key) delegates with None: behavior unchanged.
    let out = handle
        .execute_step(&request("api.pay.example", "plain"), |_attempt: u32| async {
            AttemptResult::Ok {
                payload: json!({ "ok": true }),
            }
        })
        .await;
    assert_eq!(out.result, "ok");
    handle.complete_success();

    // Re-entering a completed flow is pure replay: the peek always misses.
    let replay = fx.manager.enter_flow(&fx.desc).expect("pure replay");
    assert!(replay.is_replay_only());
    assert_eq!(replay.recorded_idempotency_key("api.pay.example#plain"), None);
}
