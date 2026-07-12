//! Tier 2 durable-flow behavior (architecture spec §4.3–4.4) against a real
//! `SqliteJournal` on a `ManualClock`, the Tier 1 `Engine`, and tokio's paused
//! clock — no wall-clock sleeps, deterministic timestamps.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use keel_core::{Engine, FlowDescriptor, FlowManager};
use keel_core_api::{AttemptResult, ENVELOPE_VERSION, Request};
use keel_journal::{
    Clock, FlowId, FlowStatus, Journal, ManualClock, ProcessId, SqliteJournal, StepKey, StepStatus,
};
use serde_json::json;
use tempfile::TempDir;

/// The fixtures' base instant (2026-07-11T00:00:00Z), so timestamps read like
/// the golden journal fixtures.
const T0: i64 = 1_783_728_000_000;

/// A test rig sharing one journal file + one clock across a manager and direct
/// inspection, exactly as a process would.
struct Rig {
    manager: FlowManager,
    journal: Arc<dyn Journal>,
    clock: ManualClock,
    _dir: TempDir,
}

fn rig(holder: &str) -> Rig {
    rig_with_clock(holder, ManualClock::new(T0))
}

fn rig_with_clock(holder: &str, clock: ManualClock) -> Rig {
    let dir = TempDir::new().unwrap();
    let journal: Arc<dyn Journal> =
        Arc::new(SqliteJournal::open(dir.path().join("journal.db"), clock.clone()).unwrap());
    let engine = Arc::new(Engine::new());
    let clock_dyn: Arc<dyn Clock> = Arc::new(clock.clone());
    let manager = FlowManager::new(
        engine,
        Arc::clone(&journal),
        clock_dyn,
        ProcessId::new(holder),
    );
    Rig {
        manager,
        journal,
        clock,
        _dir: dir,
    }
}

/// A second manager over the *same* journal file — a second "process".
fn second_manager(rig: &Rig, holder: &str) -> FlowManager {
    let clock_dyn: Arc<dyn Clock> = Arc::new(rig.clock.clone());
    FlowManager::new(
        Arc::new(Engine::new()),
        Arc::clone(&rig.journal),
        clock_dyn,
        ProcessId::new(holder),
    )
}

fn descriptor(args_hash: &str) -> FlowDescriptor {
    FlowDescriptor {
        entrypoint: "py:pipeline.ingest:main".to_owned(),
        args_hash: args_hash.to_owned(),
        explicit_key: None,
        code_hash: Some("ch-1".to_owned()),
    }
}

fn request(target: &str, args_hash: &str) -> Request {
    Request {
        v: ENVELOPE_VERSION,
        target: target.to_owned(),
        op: target.to_owned(),
        idempotent: true,
        args_hash: Some(args_hash.to_owned()),
    }
}

/// An effect that always succeeds with `payload`, counting its invocations so a
/// test can prove a replayed step did NOT call it.
fn counting_ok(
    counter: &Arc<AtomicUsize>,
    payload: serde_json::Value,
) -> impl AsyncFnMut(u32) -> AttemptResult {
    let counter = Arc::clone(counter);
    move |_attempt: u32| {
        let counter = Arc::clone(&counter);
        let payload = payload.clone();
        async move {
            counter.fetch_add(1, Ordering::SeqCst);
            AttemptResult::Ok { payload }
        }
    }
}

#[tokio::test(start_paused = true)]
async fn fresh_flow_journals_each_step_live_then_completes() {
    let r = rig("host-a:pid-1");
    let calls = Arc::new(AtomicUsize::new(0));
    let mut handle = r.manager.enter_flow(&descriptor("ah-1")).unwrap();
    let fid = handle.flow_id().clone();

    for (target, hash) in [
        ("api.source.internal", "q1"),
        ("api.enrich.internal", "q2"),
        ("api.store.internal", "w1"),
    ] {
        let out = handle
            .execute_step(
                &request(target, hash),
                counting_ok(&calls, json!({ "ok": true })),
            )
            .await;
        assert_eq!(out.result, "ok");
        assert_eq!(out.payload, Some(json!({ "ok": true })));
    }
    handle.complete_success();
    drop(handle);

    // All three effects ran live.
    assert_eq!(calls.load(Ordering::SeqCst), 3);

    // Each step is journaled ok under its (target#args_hash) key.
    for (seq, target, hash) in [
        (1, "api.source.internal", "q1"),
        (2, "api.enrich.internal", "q2"),
        (3, "api.store.internal", "w1"),
    ] {
        let key = StepKey::new(format!("{target}#{hash}"));
        let (got_key, step) = r.journal.step_at(&fid, seq).unwrap().expect("step present");
        assert_eq!(got_key, key);
        assert_eq!(step.status, StepStatus::Ok);
    }
    assert_eq!(
        r.journal.get_flow(&fid).unwrap().unwrap().status,
        FlowStatus::Completed
    );
}

#[tokio::test(start_paused = true)]
async fn live_step_records_running_before_the_terminal_outcome() {
    // At-least-once honesty: the effect observes its own step already journaled
    // `running` when it runs, proving the running-marker precedes execution.
    let r = rig("host-a:pid-1");
    let mut handle = r.manager.enter_flow(&descriptor("ah-1")).unwrap();
    let fid = handle.flow_id().clone();
    let journal = Arc::clone(&r.journal);
    let fid_for_effect = fid.clone();

    let out = handle
        .execute_step(
            &request("api.source.internal", "q1"),
            move |_attempt: u32| {
                let journal = Arc::clone(&journal);
                let fid = fid_for_effect.clone();
                async move {
                    let (_key, step) = journal
                        .step_at(&fid, 1)
                        .unwrap()
                        .expect("running step present");
                    assert_eq!(
                        step.status,
                        StepStatus::Running,
                        "journaled running pre-effect"
                    );
                    AttemptResult::Ok {
                        payload: json!({ "rows": 120 }),
                    }
                }
            },
        )
        .await;
    assert_eq!(out.result, "ok");

    let (_key, step) = r.journal.step_at(&fid, 1).unwrap().unwrap();
    assert_eq!(step.status, StepStatus::Ok, "terminal outcome recorded");
}

#[tokio::test(start_paused = true)]
async fn a_fresh_lease_blocks_a_second_holder_with_e030() {
    let r = rig("host-a:pid-1");
    let _held = r.manager.enter_flow(&descriptor("ah-1")).unwrap();

    let other = second_manager(&r, "host-b:pid-2");
    let err = other.enter_flow(&descriptor("ah-1")).unwrap_err();
    assert_eq!(err.code.as_str(), "KEEL-E030");
}

#[tokio::test(start_paused = true)]
async fn the_same_holder_may_re_enter_its_own_leased_flow() {
    // A heartbeat / re-entry by the same process must not lock itself out.
    let r = rig("host-a:pid-1");
    let first = r.manager.enter_flow(&descriptor("ah-1")).unwrap();
    let fid = first.flow_id().clone();
    drop(first);
    let again = r.manager.enter_flow(&descriptor("ah-1")).unwrap();
    assert_eq!(*again.flow_id(), fid);
}

#[test]
fn identity_maps_deterministically_to_a_flow_id() {
    let a = descriptor("ah-1").flow_id();
    let b = descriptor("ah-1").flow_id();
    assert_eq!(a, b);
    assert_ne!(a, descriptor("ah-2").flow_id());
    assert_eq!(a, FlowId::new("py:pipeline.ingest:main#ah-1#"));
}
