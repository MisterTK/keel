//! Real integration tests for [`PostgresJournal`] against a scratch Postgres
//! cluster (see `tests/support`), mirroring the `SqliteJournal` unit test
//! suite so both backends are held to the same behavioral contract.
//!
//! These tests use short *real* sleeps (a handful of hundreds of
//! milliseconds) around lease/cache TTLs. That is a deliberate, narrow
//! exception to "no real sleeps in tests": `PostgresJournal` deliberately has
//! no injectable clock (see its module doc — a fleet has no single local
//! clock to inject, so every timestamp comes from the *server's* real-time
//! clock), so there is no virtual-clock substitute available here the way
//! `ManualClock` gives the SQLite suite. Every sleep is short and this file
//! only runs when a local Postgres is available (see `support::ScratchPg`).

mod support;

use core::time::Duration;

use keel_journal::{
    CacheKey, FlowDescriptor, FlowId, FlowStatus, Journal, NewFlow, PostgresJournal, ProcessId,
    StepKey, StepKind, StepOutcome, StepStatus,
};
use support::ScratchPg;

fn sample_flow(id: &str) -> NewFlow {
    NewFlow {
        flow_id: FlowId::new(id),
        entrypoint: "py:pipeline.ingest:main".to_owned(),
        args_hash: "ah-test".to_owned(),
        code_hash: Some("ch-test".to_owned()),
    }
}

/// Skip (print, don't fail) when no local Postgres is available, exactly like
/// this repo's other native-leg tests.
macro_rules! require_pg {
    () => {
        match ScratchPg::start() {
            Some(pg) => pg,
            None => {
                eprintln!("skipping: no local `initdb` found (see tests/support)");
                return;
            }
        }
    };
}

#[test]
fn begin_flow_is_idempotent_and_stamps_running_from_the_server_clock() {
    let pg = require_pg!();
    let j = PostgresJournal::open(&pg.url()).expect("open postgres journal");
    let id = FlowId::new("01FLOW");

    j.begin_flow(&sample_flow("01FLOW")).unwrap();
    j.begin_flow(&sample_flow("01FLOW")).unwrap(); // re-begin is a no-op

    let flow = j.get_flow(&id).unwrap().expect("flow present");
    assert_eq!(flow.status, FlowStatus::Running);
    assert_eq!(
        flow.created_at, flow.updated_at,
        "begin_flow stamps both from one server-side reading of now"
    );
    assert!(flow.lease_holder.is_none());
}

#[test]
fn record_step_then_lookup_step_round_trips_every_field() {
    let pg = require_pg!();
    let j = PostgresJournal::open(&pg.url()).expect("open postgres journal");
    let flow = FlowId::new("01FLOW");
    let key = StepKey::new("api.enrich.internal#q2");
    j.begin_flow(&sample_flow("01FLOW")).unwrap();

    let outcome = StepOutcome {
        kind: StepKind::Effect,
        attempt: 2,
        status: StepStatus::Ok,
        payload: Some(vec![0x81, 0xA2, 0x6F, 0x6B, 0xC3]),
        error_class: None,
        started_at: 1_000,
        ended_at: Some(1_250),
    };
    j.record_step(&flow, 3, &key, &outcome).unwrap();

    let got = j
        .lookup_step(&flow, 3, &key)
        .unwrap()
        .expect("step present");
    assert_eq!(got, outcome);

    // Wrong key at the same seq is a miss, not a false hit.
    assert!(
        j.lookup_step(&flow, 3, &StepKey::new("other#k"))
            .unwrap()
            .is_none()
    );
}

#[test]
fn step_at_reads_the_recorded_key_regardless_of_match() {
    let pg = require_pg!();
    let j = PostgresJournal::open(&pg.url()).expect("open postgres journal");
    let flow = FlowId::new("01FLOW");
    let key = StepKey::new("api.enrich.internal#q2");
    j.begin_flow(&sample_flow("01FLOW")).unwrap();

    let outcome = StepOutcome {
        kind: StepKind::Effect,
        attempt: 1,
        status: StepStatus::Ok,
        payload: Some(vec![0x81, 0xA2, 0x6F, 0x6B, 0xC3]),
        error_class: None,
        started_at: 1_000,
        ended_at: Some(1_250),
    };
    j.record_step(&flow, 3, &key, &outcome).unwrap();

    let (got_key, got) = j.step_at(&flow, 3).unwrap().expect("step present");
    assert_eq!(got_key, key);
    assert_eq!(got, outcome);
    assert!(j.step_at(&flow, 9).unwrap().is_none());
}

#[test]
fn record_step_upsert_preserves_started_at_on_running_to_ok() {
    let pg = require_pg!();
    let j = PostgresJournal::open(&pg.url()).expect("open postgres journal");
    let flow = FlowId::new("01FLOW");
    let key = StepKey::new("api.store.internal#w1");
    j.begin_flow(&sample_flow("01FLOW")).unwrap();

    let running = StepOutcome {
        kind: StepKind::Effect,
        attempt: 1,
        status: StepStatus::Running,
        payload: None,
        error_class: None,
        started_at: 1_100,
        ended_at: None,
    };
    j.record_step(&flow, 4, &key, &running).unwrap();

    let done = StepOutcome {
        status: StepStatus::Ok,
        payload: Some(vec![0xC0]),
        ended_at: Some(1_900),
        started_at: 1_555, // a re-record proposing a later start...
        ..running.clone()
    };
    j.record_step(&flow, 4, &key, &done).unwrap();

    let got = j.lookup_step(&flow, 4, &key).unwrap().unwrap();
    assert_eq!(got.status, StepStatus::Ok);
    assert_eq!(got.started_at, 1_100, "original start is preserved");
    assert_eq!(got.ended_at, Some(1_900));
}

#[test]
fn completed_flow_is_never_demoted() {
    let pg = require_pg!();
    let j = PostgresJournal::open(&pg.url()).expect("open postgres journal");

    let done = FlowId::new("01DONE");
    j.begin_flow(&sample_flow("01DONE")).unwrap();
    j.complete_flow(&done, FlowStatus::Completed).unwrap();

    j.complete_flow(&done, FlowStatus::Failed).unwrap();
    assert_eq!(
        j.get_flow(&done).unwrap().unwrap().status,
        FlowStatus::Completed,
        "completed must stay completed"
    );
    j.complete_flow(&done, FlowStatus::Dead).unwrap();
    assert_eq!(
        j.get_flow(&done).unwrap().unwrap().status,
        FlowStatus::Completed
    );
    assert!(
        j.incomplete_flows(true).unwrap().is_empty(),
        "must not reopen for recovery"
    );

    // Failed → completed (a resume that finally succeeds) is still allowed.
    let retried = FlowId::new("02RETRY");
    j.begin_flow(&sample_flow("02RETRY")).unwrap();
    j.complete_flow(&retried, FlowStatus::Failed).unwrap();
    j.complete_flow(&retried, FlowStatus::Completed).unwrap();
    assert_eq!(
        j.get_flow(&retried).unwrap().unwrap().status,
        FlowStatus::Completed
    );
}

#[test]
fn incomplete_flows_splits_on_lease_expiry_against_the_servers_clock() {
    let pg = require_pg!();
    let j = PostgresJournal::open(&pg.url()).expect("open postgres journal");
    let flow = FlowId::new("01FLOW");
    let holder = ProcessId::new("host-a:pid-1");
    j.begin_flow(&sample_flow("01FLOW")).unwrap();
    assert!(
        j.acquire_lease(&flow, &holder, Duration::from_millis(150))
            .unwrap()
    );

    // Lease valid: shows as actively-leased, not as a recovery candidate.
    assert!(j.incomplete_flows(true).unwrap().is_empty());
    assert_eq!(j.incomplete_flows(false).unwrap().len(), 1);

    // No virtual clock to advance against a live server — sleep past the
    // short real TTL (see module doc).
    std::thread::sleep(Duration::from_millis(300));

    let recoverable = j.incomplete_flows(true).unwrap();
    assert_eq!(recoverable.len(), 1);
    assert_eq!(recoverable[0].flow_id, flow);
    assert!(j.incomplete_flows(false).unwrap().is_empty());
}

#[test]
fn lease_contention_between_two_fleet_processes_arbitrates_on_the_servers_clock() {
    // Two `PostgresJournal` handles against the SAME cluster model two
    // processes in a fleet, arbitrating over the same `flows` row — the
    // scenario `acquire_lease`'s DB-time comparison exists for.
    let pg = require_pg!();
    let a = PostgresJournal::open(&pg.url()).expect("open postgres journal (process a)");
    let b = PostgresJournal::open(&pg.url()).expect("open postgres journal (process b)");
    let flow = FlowId::new("01FLOW");
    let holder_a = ProcessId::new("host-a:pid-1");
    let holder_b = ProcessId::new("host-b:pid-2");
    a.begin_flow(&sample_flow("01FLOW")).unwrap();

    assert!(
        a.acquire_lease(&flow, &holder_a, Duration::from_millis(200))
            .unwrap()
    );
    assert!(
        !b.acquire_lease(&flow, &holder_b, Duration::from_millis(200))
            .unwrap(),
        "second process must lose while the lease is valid"
    );
    // The holder may re-take (heartbeat) before expiry.
    assert!(
        a.acquire_lease(&flow, &holder_a, Duration::from_millis(200))
            .unwrap()
    );

    std::thread::sleep(Duration::from_millis(400));
    assert!(
        b.acquire_lease(&flow, &holder_b, Duration::from_millis(200))
            .unwrap(),
        "an expired lease is stealable by another process"
    );
}

#[test]
fn cache_put_get_honors_ttl_against_the_servers_clock() {
    let pg = require_pg!();
    let j = PostgresJournal::open(&pg.url()).expect("open postgres journal");
    let key = CacheKey::new("api.catalog.internal#g1");

    j.put_cache(&key, b"payload", Duration::from_millis(300))
        .unwrap();
    assert_eq!(j.get_cache(&key).unwrap().as_deref(), Some(&b"payload"[..]));

    std::thread::sleep(Duration::from_millis(500));
    assert!(j.get_cache(&key).unwrap().is_none(), "past the short TTL");
}

#[test]
fn reopening_an_existing_database_keeps_its_rows_and_is_race_safe() {
    // `open` re-applies the (idempotent) schema on every connect; opening
    // twice — and, more importantly, opening from many threads at once, the
    // fleet-boot scenario — must not fail with "relation already exists".
    let pg = require_pg!();
    {
        let j = PostgresJournal::open(&pg.url()).expect("first open");
        j.begin_flow(&sample_flow("01FLOW")).unwrap();
    }
    let url = pg.url();
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let url = url.clone();
            std::thread::spawn(move || {
                PostgresJournal::open(&url)
                    .map(|_| ())
                    .map_err(|e| e.to_string())
            })
        })
        .collect();
    for h in handles {
        h.join()
            .unwrap()
            .expect("concurrent reopen must not race on schema creation");
    }
    let reopened = PostgresJournal::open(&pg.url()).expect("reopen");
    assert!(reopened.get_flow(&FlowId::new("01FLOW")).unwrap().is_some());
}

#[test]
fn flows_by_entrypoint_returns_every_status_ordered_by_created_at() {
    let pg = require_pg!();
    let j = PostgresJournal::open(&pg.url()).expect("open postgres journal");

    // A flow under a DIFFERENT entrypoint must never appear below.
    let other = NewFlow {
        flow_id: FlowId::new("00OTHER"),
        entrypoint: "py:other.module:main".to_owned(),
        args_hash: "ah-other".to_owned(),
        code_hash: None,
    };
    j.begin_flow(&other).unwrap();

    j.begin_flow(&sample_flow("01FIRST")).unwrap();
    std::thread::sleep(Duration::from_millis(20));
    j.begin_flow(&sample_flow("02SECOND")).unwrap();
    std::thread::sleep(Duration::from_millis(20));
    let third = FlowId::new("03THIRD");
    j.begin_flow(&sample_flow("03THIRD")).unwrap();
    // Unlike `incomplete_flows`, a non-`running` flow must still show up.
    j.complete_flow(&third, FlowStatus::Failed).unwrap();

    let flows: Vec<FlowDescriptor> = j.flows_by_entrypoint("py:pipeline.ingest:main").unwrap();
    let ids: Vec<&str> = flows.iter().map(|f| f.flow_id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["01FIRST", "02SECOND", "03THIRD"],
        "every status for this entrypoint, ordered by created_at"
    );
    assert_eq!(flows[0].status, FlowStatus::Running);
    assert_eq!(
        flows[2].status,
        FlowStatus::Failed,
        "failed flows are included too"
    );

    assert!(
        j.flows_by_entrypoint("py:nonexistent:main")
            .unwrap()
            .is_empty(),
        "an entrypoint with no flows reads as empty, not an error"
    );
}

#[test]
fn steps_for_flow_returns_every_step_in_seq_order_with_raw_payload() {
    let pg = require_pg!();
    let j = PostgresJournal::open(&pg.url()).expect("open postgres journal");
    let flow = FlowId::new("01FLOW");
    j.begin_flow(&sample_flow("01FLOW")).unwrap();

    let step0 = StepOutcome {
        kind: StepKind::Effect,
        attempt: 1,
        status: StepStatus::Ok,
        payload: Some(vec![0x81, 0xA2, 0x6F, 0x6B, 0xC3]),
        error_class: None,
        started_at: 1_000,
        ended_at: Some(1_005),
    };
    let step1 = StepOutcome {
        status: StepStatus::Running,
        payload: None,
        started_at: 1_010,
        ended_at: None,
        ..step0.clone()
    };
    // Recorded out of seq order, to prove the read orders by `seq`, not
    // insertion order.
    j.record_step(&flow, 1, &StepKey::new("api.b.internal#w1"), &step1)
        .unwrap();
    j.record_step(&flow, 0, &StepKey::new("api.a.internal#w0"), &step0)
        .unwrap();

    let got = j.steps_for_flow(&flow).unwrap();
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].0, StepKey::new("api.a.internal#w0"));
    assert_eq!(got[0].1, step0, "payload stays raw/undecoded");
    assert_eq!(got[1].0, StepKey::new("api.b.internal#w1"));
    assert_eq!(got[1].1, step1);

    let empty = FlowId::new("02EMPTY");
    j.begin_flow(&sample_flow("02EMPTY")).unwrap();
    assert!(
        j.steps_for_flow(&empty).unwrap().is_empty(),
        "a flow with no steps yet reads as empty, not an error"
    );
}
