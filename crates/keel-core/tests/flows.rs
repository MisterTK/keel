//! Tier 2 durable-flow behavior (architecture spec §4.3–4.4) against a real
//! `SqliteJournal` on a `ManualClock`, the Tier 1 `Engine`, and tokio's paused
//! clock — no wall-clock sleeps, deterministic timestamps.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use keel_core::{Engine, FlowConfig, FlowDescriptor, FlowManager};
use keel_core_api::{AttemptResult, ENVELOPE_VERSION, Request};
use keel_journal::{
    Clock, FlowId, FlowStatus, Journal, ManualClock, ProcessId, SqliteJournal, StepKey, StepStatus,
    SystemClock,
};
use rusqlite::Connection;
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
    descriptor_ch(args_hash, "ch-1")
}

fn descriptor_ch(args_hash: &str, code_hash: &str) -> FlowDescriptor {
    FlowDescriptor {
        entrypoint: "py:pipeline.ingest:main".to_owned(),
        args_hash: args_hash.to_owned(),
        explicit_key: None,
        code_hash: Some(code_hash.to_owned()),
    }
}

/// A rig whose engine is configured with a `flows.on_nondeterminism` response.
fn rig_with_response(holder: &str, response: &str) -> Rig {
    let dir = TempDir::new().unwrap();
    let clock = ManualClock::new(T0);
    let journal: Arc<dyn Journal> =
        Arc::new(SqliteJournal::open(dir.path().join("journal.db"), clock.clone()).unwrap());
    let engine = Engine::new();
    engine
        .configure(&json!({ "flows": { "on_nondeterminism": response } }))
        .unwrap();
    let clock_dyn: Arc<dyn Clock> = Arc::new(clock.clone());
    let manager = FlowManager::new(
        Arc::new(engine),
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

/// A rig whose flow manager caps flow-level attempts at `max_attempts`.
fn rig_with_config(holder: &str, max_attempts: u32) -> Rig {
    let dir = TempDir::new().unwrap();
    let clock = ManualClock::new(T0);
    let journal: Arc<dyn Journal> =
        Arc::new(SqliteJournal::open(dir.path().join("journal.db"), clock.clone()).unwrap());
    let clock_dyn: Arc<dyn Clock> = Arc::new(clock.clone());
    let manager = FlowManager::with_config(
        Arc::new(Engine::new()),
        Arc::clone(&journal),
        clock_dyn,
        ProcessId::new(holder),
        FlowConfig {
            lease_ttl: Duration::from_secs(30),
            max_attempts,
        },
    );
    Rig {
        manager,
        journal,
        clock,
        _dir: dir,
    }
}

/// Record step 1 under `api.a#q1`, then "crash" and let the lease expire, so a
/// resume that runs a differently-keyed step at seq 1 diverges.
async fn seed_divergence(r: &Rig, desc: &FlowDescriptor) {
    {
        let mut h = r.manager.enter_flow(desc).unwrap();
        let sink = Arc::new(AtomicUsize::new(0));
        h.execute_step(
            &request("api.a", "q1"),
            counting_ok(&sink, json!({ "v": 1 })),
        )
        .await;
    }
    r.clock.advance(31_000);
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

/// A request whose key is `py:time.time#-`, matching the golden fixtures'
/// virtualized time step (kind is invisible to replay, which matches on key).
fn time_request() -> Request {
    Request {
        v: ENVELOPE_VERSION,
        target: "py:time.time".to_owned(),
        op: "py:time.time".to_owned(),
        idempotent: true,
        args_hash: None,
    }
}

/// Build a journal db from a checked-in golden fixture: the frozen schema plus
/// the fixture's bit-identical `INSERT`s (`conformance/fixtures/journal/`).
fn build_fixture_db(dir: &TempDir, fixture: &str) -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let schema = std::fs::read_to_string(root.join("contracts/journal.sql")).unwrap();
    let sql =
        std::fs::read_to_string(root.join("conformance/fixtures/journal").join(fixture)).unwrap();
    let path = dir.path().join("journal.db");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(&schema).unwrap();
    conn.execute_batch(&sql).unwrap();
    path
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

#[tokio::test(start_paused = true)]
async fn crash_after_step_three_resumes_substituting_completed_steps() {
    // Run a 5-step flow, "crash" (drop the handle) after step 3, then resume on
    // a fresh engine over the same journal: steps 1-3 are substituted from the
    // journal (their effects never re-run), 4-5 execute live.
    let r = rig("host-a:pid-1");
    let steps = [
        ("api.a.internal", "1"),
        ("api.b.internal", "2"),
        ("api.c.internal", "3"),
        ("api.d.internal", "4"),
        ("api.e.internal", "5"),
    ];

    let run1 = Arc::new(AtomicUsize::new(0));
    let fid;
    {
        let mut handle = r.manager.enter_flow(&descriptor("ah-1")).unwrap();
        fid = handle.flow_id().clone();
        for (i, (target, hash)) in steps.iter().take(3).enumerate() {
            let n = (i + 1) as u64;
            let out = handle
                .execute_step(
                    &request(target, hash),
                    counting_ok(&run1, json!({ "step": n })),
                )
                .await;
            assert_eq!(out.payload, Some(json!({ "step": n })));
        }
        // handle dropped here WITHOUT complete() — a crash mid-flow.
    }
    assert_eq!(
        run1.load(Ordering::SeqCst),
        3,
        "three live steps before crash"
    );

    // Lease (30s) expires, so a second process may steal it.
    r.clock.advance(31_000);

    let run2 = Arc::new(AtomicUsize::new(0));
    let manager2 = second_manager(&r, "host-b:pid-2");
    let mut handle = manager2.enter_flow(&descriptor("ah-1")).unwrap();
    assert_eq!(
        *handle.flow_id(),
        fid,
        "same identity resumes the same flow"
    );
    for (i, (target, hash)) in steps.iter().enumerate() {
        let n = (i + 1) as u64;
        let out = handle
            .execute_step(
                &request(target, hash),
                counting_ok(&run2, json!({ "step": n })),
            )
            .await;
        assert_eq!(out.result, "ok");
        assert_eq!(
            out.payload,
            Some(json!({ "step": n })),
            "replayed payload matches"
        );
    }
    handle.complete_success();

    assert_eq!(
        run2.load(Ordering::SeqCst),
        2,
        "steps 1-3 substituted without side effect; only 4-5 ran live"
    );
    assert_eq!(
        r.journal.get_flow(&fid).unwrap().unwrap().status,
        FlowStatus::Completed
    );
}

#[tokio::test(start_paused = true)]
async fn resumes_the_interrupted_flow_golden_fixture() {
    // The interrupted-flow golden fixture: steps 1-3 recorded (one a `time`
    // step), step 4 crashed mid-flight (`running`), lease expired. Resume must
    // substitute 1-3, re-execute the crashed step 4 live, then run step 5 live.
    // A clock past the fixture lease expiry (T0+30s), so the flow is a
    // recovery candidate.
    let now = T0 + 60_000;
    let dir = TempDir::new().unwrap();
    let path = build_fixture_db(&dir, "interrupted-flow.sql");
    let clock = ManualClock::new(now);
    let journal: Arc<dyn Journal> = Arc::new(SqliteJournal::open(&path, clock.clone()).unwrap());
    let clock_dyn: Arc<dyn Clock> = Arc::new(clock.clone());
    let manager = FlowManager::new(
        Arc::new(Engine::new()),
        Arc::clone(&journal),
        clock_dyn,
        ProcessId::new("host-b:pid-9"),
    );

    let candidates = journal.incomplete_flows(true).unwrap();
    assert_eq!(
        candidates.len(),
        1,
        "the interrupted flow is a recovery candidate"
    );
    let mut handle = manager
        .resume_flow(&candidates[0], Some("ch-9b2e44"))
        .unwrap();

    let live = Arc::new(AtomicUsize::new(0));
    let o1 = handle
        .execute_step(
            &request("api.source.internal", "q1"),
            counting_ok(&live, json!(null)),
        )
        .await;
    assert_eq!(o1.payload, Some(json!({ "rows": 120 })));
    let o2 = handle
        .execute_step(&time_request(), counting_ok(&live, json!(null)))
        .await;
    // Substituted verbatim: the fixture's exact journaled uint32 (bytes
    // 0xCE6A518600 = 1_783_727_616), i.e. the recorded clock read.
    assert_eq!(
        o2.payload,
        Some(json!(1_783_727_616)),
        "virtualized time replays"
    );
    let o3 = handle
        .execute_step(
            &request("api.enrich.internal", "q2"),
            counting_ok(&live, json!(null)),
        )
        .await;
    assert_eq!(o3.payload, Some(json!({ "ok": true })));
    assert_eq!(
        live.load(Ordering::SeqCst),
        0,
        "steps 1-3 substituted, no effect run"
    );

    let o4 = handle
        .execute_step(
            &request("api.store.internal", "w1"),
            counting_ok(&live, json!({ "stored": true })),
        )
        .await;
    assert_eq!(o4.result, "ok");
    let o5 = handle
        .execute_step(
            &request("api.notify.internal", "n1"),
            counting_ok(&live, json!({ "sent": true })),
        )
        .await;
    assert_eq!(o5.result, "ok");
    assert_eq!(
        live.load(Ordering::SeqCst),
        2,
        "crashed step 4 + fresh step 5 ran live"
    );

    handle.complete_success();
    assert_eq!(
        journal.get_flow(handle.flow_id()).unwrap().unwrap().status,
        FlowStatus::Completed
    );
}

#[tokio::test(start_paused = true)]
async fn divergence_fails_with_e031_naming_expected_and_actual() {
    // Default policy is `fail`.
    let r = rig("host-a:pid-1");
    seed_divergence(&r, &descriptor("ah-1")).await;

    let calls = Arc::new(AtomicUsize::new(0));
    let mut handle = r.manager.enter_flow(&descriptor("ah-1")).unwrap();
    let out = handle
        .execute_step(
            &request("api.b", "q2"),
            counting_ok(&calls, json!({ "v": 2 })),
        )
        .await;

    assert_eq!(out.result, "error");
    let err = out.error.expect("divergence error");
    assert_eq!(err.code.as_str(), "KEEL-E031");
    assert!(err.message.contains("api.a#q1"), "names the expected step");
    assert!(err.message.contains("api.b#q2"), "names the observed step");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "fail never runs the effect"
    );
}

#[tokio::test(start_paused = true)]
async fn warn_continues_live_and_journals_a_marker() {
    let r = rig_with_response("host-a:pid-1", "warn");
    seed_divergence(&r, &descriptor("ah-1")).await;

    let calls = Arc::new(AtomicUsize::new(0));
    let mut handle = r.manager.enter_flow(&descriptor("ah-1")).unwrap();
    let fid = handle.flow_id().clone();
    let out = handle
        .execute_step(
            &request("api.b", "q2"),
            counting_ok(&calls, json!({ "v": 2 })),
        )
        .await;

    assert_eq!(out.result, "ok", "warn continues live");
    assert_eq!(out.payload, Some(json!({ "v": 2 })));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the divergent step ran live"
    );
    // seq 1 now holds the branch marker; the live re-execution lands at seq 2.
    let (marker_key, marker) = r.journal.step_at(&fid, 1).unwrap().unwrap();
    assert_eq!(marker_key, StepKey::new("flow:branch:warn"));
    assert_eq!(marker.kind, keel_journal::StepKind::Marker);
    let (live_key, _) = r.journal.step_at(&fid, 2).unwrap().unwrap();
    assert_eq!(live_key, StepKey::new("api.b#q2"));
}

#[tokio::test(start_paused = true)]
async fn branch_starts_fresh_and_preserves_the_old_record() {
    let r = rig_with_response("host-a:pid-1", "branch");
    seed_divergence(&r, &descriptor("ah-1")).await;

    let calls = Arc::new(AtomicUsize::new(0));
    let mut handle = r.manager.enter_flow(&descriptor("ah-1")).unwrap();
    let fid = handle.flow_id().clone();
    let out = handle
        .execute_step(
            &request("api.b", "q2"),
            counting_ok(&calls, json!({ "v": 2 })),
        )
        .await;

    assert_eq!(out.result, "ok", "branch runs a fresh attempt live");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    // The abandoned run's original step at seq 1 is preserved for audit.
    let (original_key, _) = r.journal.step_at(&fid, 1).unwrap().unwrap();
    assert_eq!(original_key, StepKey::new("api.a#q1"), "old record kept");
    // The fresh attempt wrote its live step in the high branch lane.
    let (live_key, _) = r.journal.step_at(&fid, 1_000_002).unwrap().unwrap();
    assert_eq!(live_key, StepKey::new("api.b#q2"));
}

#[tokio::test(start_paused = true)]
async fn code_hash_mismatch_downgrades_fail_to_warn() {
    // Engine policy is the default `fail`, but the recorded code_hash differs
    // from the one now deployed — a changed deploy is expected to diverge.
    let r = rig("host-a:pid-1");
    seed_divergence(&r, &descriptor_ch("ah-1", "ch-1")).await;

    let calls = Arc::new(AtomicUsize::new(0));
    let mut handle = r
        .manager
        .enter_flow(&descriptor_ch("ah-1", "ch-2"))
        .unwrap();
    let out = handle
        .execute_step(
            &request("api.b", "q2"),
            counting_ok(&calls, json!({ "v": 2 })),
        )
        .await;

    assert_eq!(
        out.result, "ok",
        "fenced divergence downgrades to warn (live)"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn attempt_cap_marks_a_poison_flow_dead_with_e032() {
    let r = rig_with_config("host-a:pid-1", 2);
    let d = descriptor("ah-1");

    // Two failing attempts are within the cap (complete_failed clears the lease,
    // so the next attempt can re-enter immediately).
    for _ in 0..2 {
        let mut h = r.manager.enter_flow(&d).expect("attempt within cap");
        h.complete_failed();
    }

    // The third entry exceeds the cap: the flow is marked dead and refused.
    let err = r.manager.enter_flow(&d).unwrap_err();
    assert_eq!(err.code.as_str(), "KEEL-E032");
    assert_eq!(
        r.journal.get_flow(&d.flow_id()).unwrap().unwrap().status,
        FlowStatus::Dead
    );
    // A dead flow is never a recovery candidate, and re-entry stays refused.
    assert!(r.journal.incomplete_flows(true).unwrap().is_empty());
    assert_eq!(
        r.manager.enter_flow(&d).unwrap_err().code.as_str(),
        "KEEL-E032"
    );
}

#[tokio::test(start_paused = true)]
async fn a_dead_flow_from_the_golden_fixture_is_never_resumed() {
    let dir = TempDir::new().unwrap();
    let path = build_fixture_db(&dir, "dead-flow.sql");
    let clock = ManualClock::new(T0 + 60_000);
    let journal: Arc<dyn Journal> = Arc::new(SqliteJournal::open(&path, clock.clone()).unwrap());
    let clock_dyn: Arc<dyn Clock> = Arc::new(clock.clone());
    let manager = FlowManager::new(
        Arc::new(Engine::new()),
        Arc::clone(&journal),
        clock_dyn,
        ProcessId::new("host-b:pid-9"),
    );

    let dead_id = FlowId::new("01JZWY0A0000000000000003");
    let desc = journal
        .get_flow(&dead_id)
        .unwrap()
        .expect("dead flow present");
    assert_eq!(desc.status, FlowStatus::Dead);

    let err = manager.resume_flow(&desc, Some("ch-1a7f02")).unwrap_err();
    assert_eq!(err.code.as_str(), "KEEL-E032");
    assert!(
        journal.incomplete_flows(true).unwrap().is_empty(),
        "a dead flow is not a recovery candidate"
    );
}

/// The lease renewer is a dedicated OS thread on real wall-clock time (so it
/// works under the synchronous bindings, whose current-thread runtime is not
/// polled between calls). Proven on a real clock with a tiny ttl and small real
/// sleeps: a renewal pushes the lease expiry forward while the handle is held.
#[test]
fn the_heartbeat_renews_the_lease_on_a_real_clock() {
    let dir = TempDir::new().unwrap();
    let journal: Arc<dyn Journal> =
        Arc::new(SqliteJournal::open(dir.path().join("journal.db"), SystemClock).unwrap());
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let manager = FlowManager::with_config(
        Arc::new(Engine::new()),
        Arc::clone(&journal),
        clock,
        ProcessId::new("host-a:pid-1"),
        FlowConfig {
            lease_ttl: Duration::from_millis(80), // period = ttl/2 = 40ms
            max_attempts: 3,
        },
    );
    let handle = manager.enter_flow(&descriptor("ah-1")).unwrap();
    let fid = handle.flow_id().clone();
    let initial = journal
        .get_flow(&fid)
        .unwrap()
        .unwrap()
        .lease_expires
        .unwrap();

    // Wait through several renewal periods; the std-thread heartbeat renews
    // without any runtime being polled.
    std::thread::sleep(Duration::from_millis(220));

    let renewed = journal
        .get_flow(&fid)
        .unwrap()
        .unwrap()
        .lease_expires
        .unwrap();
    assert!(
        renewed > initial,
        "heartbeat pushed the lease expiry forward: {renewed} > {initial}"
    );
    drop(handle);
}

/// A local wall-clock jump forward past the lease TTL must NOT, by itself,
/// make a still-legitimate holder believe it lost the lease. `lease_lost`
/// judges local freshness on a monotonic clock (architecture-spec §6), never
/// by re-deriving expiry from the injected wall clock — otherwise an NTP
/// correction on this very process could make it fence its own live steps for
/// no reason, even though nobody else has touched the lease
/// (`journal.get_flow(&fid).lease_holder` is still us).
#[tokio::test(start_paused = true)]
async fn a_forward_wall_clock_jump_alone_does_not_fence_a_live_step() {
    let r = rig("host-a:pid-1");
    let mut handle = r.manager.enter_flow(&descriptor("ah-1")).unwrap();
    let fid = handle.flow_id().clone();

    // Simulate an NTP correction: the injected wall clock leaps past the
    // lease TTL, but nobody has stolen the lease.
    r.clock.advance(31_000);
    assert_eq!(
        r.journal
            .get_flow(&fid)
            .unwrap()
            .unwrap()
            .lease_holder
            .unwrap()
            .as_str(),
        "host-a:pid-1",
        "still the recorded holder; nobody stole it"
    );

    let calls = Arc::new(AtomicUsize::new(0));
    let out = handle
        .execute_step(
            &request("api.charge.internal", "c1"),
            counting_ok(&calls, json!({ "charged": true })),
        )
        .await;

    assert_eq!(
        out.result, "ok",
        "no theft occurred; a wall-clock jump alone must not fence the step"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    handle.complete_success();
}

/// A live step whose lease was stolen by another process fences with KEEL-E030
/// and does NOT fire the effect — the split-brain double-execution the lease
/// exists to prevent (findings flow.rs:851/846).
#[tokio::test(start_paused = true)]
async fn a_live_step_fences_when_its_lease_was_stolen() {
    let r = rig("host-a:pid-1");
    let mut handle = r.manager.enter_flow(&descriptor("ah-1")).unwrap();
    let fid = handle.flow_id().clone();

    // The lease lapses and a second process legitimately steals it.
    r.clock.advance(31_000);
    assert!(
        r.journal
            .acquire_lease(
                &fid,
                &ProcessId::new("host-b:pid-2"),
                Duration::from_secs(30)
            )
            .unwrap(),
        "host-b steals the expired lease"
    );

    // host-a now tries to run a live effect: it must fence, not double-execute.
    let calls = Arc::new(AtomicUsize::new(0));
    let out = handle
        .execute_step(
            &request("api.charge.internal", "c1"),
            counting_ok(&calls, json!({ "charged": true })),
        )
        .await;

    assert_eq!(out.result, "error");
    assert_eq!(
        out.error.expect("terminal error").code.as_str(),
        "KEEL-E030"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "effect must not fire once the lease is lost"
    );
    drop(handle);
}

#[tokio::test(start_paused = true)]
async fn journal_time_and_random_replay_recorded_values() {
    let r = rig("host-a:pid-1");
    // Run 1: record a virtualized time read and a random draw, then crash.
    {
        let mut handle = r.manager.enter_flow(&descriptor("ah-1")).unwrap();
        assert_eq!(handle.journal_time("py:time.time#-", 1_000).unwrap(), 1_000);
        assert_eq!(
            handle
                .journal_random("py:random.random#-", vec![1, 2, 3])
                .unwrap(),
            vec![1, 2, 3]
        );
    }
    r.clock.advance(31_000);

    // Run 2: resume — the recorded values are substituted, not the new args, so
    // the resumed flow observes the same "now" and randomness (spec §4.4).
    let mut handle = r.manager.enter_flow(&descriptor("ah-1")).unwrap();
    assert_eq!(
        handle.journal_time("py:time.time#-", 9_999).unwrap(),
        1_000,
        "time replays"
    );
    assert_eq!(
        handle
            .journal_random("py:random.random#-", vec![9, 9, 9])
            .unwrap(),
        vec![1, 2, 3],
        "random replays"
    );
    handle.complete_success();
}

#[tokio::test(start_paused = true)]
async fn re_entering_a_completed_flow_is_pure_replay() {
    // Carried review item 1: re-entering a completed flow must be pure replay —
    // no lease error (KEEL-E030), no attempt consumed, no effect re-fired — with
    // every recorded step substituted verbatim.
    let r = rig("host-a:pid-1");
    let calls = Arc::new(AtomicUsize::new(0));
    let steps = [("api.a.internal", "1"), ("api.b.internal", "2")];

    let fid;
    {
        let mut handle = r.manager.enter_flow(&descriptor("ah-1")).unwrap();
        fid = handle.flow_id().clone();
        assert_eq!(handle.entry_status(), FlowStatus::Running);
        assert!(!handle.is_replay_only());
        for (target, hash) in steps {
            handle
                .execute_step(
                    &request(target, hash),
                    counting_ok(&calls, json!({ "t": target })),
                )
                .await;
        }
        handle.complete_success();
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "both effects ran live once"
    );

    // Re-enter the SAME identity: the flow is completed, so this is pure replay.
    let mut replay = r.manager.enter_flow(&descriptor("ah-1")).unwrap();
    assert_eq!(*replay.flow_id(), fid);
    assert_eq!(replay.entry_status(), FlowStatus::Completed);
    assert!(replay.is_replay_only(), "completed re-entry is replay-only");
    for (target, hash) in steps {
        let out = replay
            .execute_step(
                &request(target, hash),
                counting_ok(&calls, json!({ "SHOULD_NOT_RUN": true })),
            )
            .await;
        assert_eq!(out.result, "ok");
        assert_eq!(
            out.payload,
            Some(json!({ "t": target })),
            "recorded payload substituted"
        );
    }
    replay.complete_success();

    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "replay fired NO effects (still 2 from run 1)"
    );
    // The seq-0 attempt counter still reads 1 (the single initial entry): the
    // completed re-entry consumed no additional attempt.
    let (_k, marker) = r.journal.step_at(&fid, 0).unwrap().expect("attempt marker");
    assert_eq!(marker.attempt, 1, "completed re-entry consumes no attempt");
    assert_eq!(
        r.journal.get_flow(&fid).unwrap().unwrap().status,
        FlowStatus::Completed
    );
}

#[tokio::test(start_paused = true)]
async fn replay_only_refuses_an_unrecorded_step_with_e031() {
    // A completed flow whose code grew a step: the replay handle reaches an
    // unrecorded seq and refuses it (KEEL-E031) rather than firing the effect.
    let r = rig("host-a:pid-1");
    let calls = Arc::new(AtomicUsize::new(0));
    {
        let mut handle = r.manager.enter_flow(&descriptor("ah-1")).unwrap();
        handle
            .execute_step(
                &request("api.a.internal", "1"),
                counting_ok(&calls, json!(1)),
            )
            .await;
        handle.complete_success();
    }

    let mut replay = r.manager.enter_flow(&descriptor("ah-1")).unwrap();
    // Step 1 replays fine.
    let ok = replay
        .execute_step(
            &request("api.a.internal", "1"),
            counting_ok(&calls, json!(9)),
        )
        .await;
    assert_eq!(ok.payload, Some(json!(1)));
    // Step 2 has no record on this completed flow → refused, effect not run.
    let miss = replay
        .execute_step(
            &request("api.new.internal", "2"),
            counting_ok(&calls, json!(9)),
        )
        .await;
    assert_eq!(miss.result, "error");
    assert_eq!(miss.error.expect("miss error").code.as_str(), "KEEL-E031");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "only run-1's effect ever ran"
    );
}

#[test]
fn identity_maps_deterministically_to_a_flow_id() {
    let a = descriptor("ah-1").flow_id();
    let b = descriptor("ah-1").flow_id();
    assert_eq!(a, b);
    assert_ne!(a, descriptor("ah-2").flow_id());
    assert_eq!(a, FlowId::new("py:pipeline.ingest:main#ah-1#"));
}
