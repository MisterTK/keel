//! Golden-fixture compatibility: `SqliteJournal` must read the exact journal
//! shapes the checked-in fixtures promise (`conformance/fixtures/journal/`).
//!
//! `build_fixtures.py` writes those `.sql` files over `contracts/journal.sql`
//! into `.gen/*.db`; this test does the same construction in-process (same
//! schema, same fixture SQL, same bytes) so it is self-contained and always
//! runnable under `cargo test` without a prior Python step, then opens the
//! result through the real backend and asserts each fixture's story.

use std::path::{Path, PathBuf};

use keel_journal::{
    ErrorClass, FlowId, FlowStatus, Journal, ManualClock, SqliteJournal, StepKey, StepStatus,
};
use rusqlite::Connection;
use tempfile::TempDir;

/// A clock reading 60s past the fixtures' base T0 (2026-07-11T00:00:00Z), so
/// the interrupted flow's lease (expiring at T0+30s) reads as expired.
const NOW: i64 = 1_783_728_060_000;

fn repo_file(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(relative)
}

/// Rebuild a fixture database exactly as `build_fixtures.py` does — the frozen
/// schema, then the fixture's inserts — and open it through `SqliteJournal`.
fn open_fixture(name: &str) -> (TempDir, SqliteJournal<ManualClock>) {
    let schema = std::fs::read_to_string(repo_file("contracts/journal.sql")).unwrap();
    let fixture = std::fs::read_to_string(repo_file(&format!(
        "conformance/fixtures/journal/{name}.sql"
    )))
    .unwrap();

    let dir = TempDir::new().unwrap();
    let path = dir.path().join(format!("{name}.db"));
    {
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(&schema).unwrap();
        conn.execute_batch(&fixture).unwrap();
    }
    let journal = SqliteJournal::open(&path, ManualClock::new(NOW)).unwrap();
    (dir, journal)
}

#[test]
fn completed_flow_is_never_offered_for_recovery() {
    let (_dir, j) = open_fixture("completed-flow");
    let id = FlowId::new("01JZWY0A0000000000000001");

    let flow = j.get_flow(&id).unwrap().expect("completed flow present");
    assert_eq!(flow.status, FlowStatus::Completed);
    assert!(flow.lease_holder.is_none());

    assert!(
        j.incomplete_flows(true).unwrap().is_empty(),
        "a completed flow is not a recovery candidate"
    );

    // Its steps are still readable (step 3 succeeded on its 2nd attempt).
    let step3 = j
        .lookup_step(&id, 3, &StepKey::new("api.enrich.internal#q2"))
        .unwrap()
        .expect("step 3 present");
    assert_eq!(step3.status, StepStatus::Ok);
    assert_eq!(step3.attempt, 2);
}

#[test]
fn interrupted_flow_is_a_recovery_candidate_with_readable_steps() {
    let (_dir, j) = open_fixture("interrupted-flow");
    let id = FlowId::new("01JZWY0A0000000000000002");

    // Expired lease → offered for recovery; still actively-leased set is empty.
    let recoverable = j.incomplete_flows(true).unwrap();
    assert_eq!(recoverable.len(), 1);
    assert_eq!(recoverable[0].flow_id, id);
    assert!(j.incomplete_flows(false).unwrap().is_empty());

    // A completed step replays from its recorded payload...
    let step1 = j
        .lookup_step(&id, 1, &StepKey::new("api.source.internal#q1"))
        .unwrap()
        .expect("step 1 present");
    assert_eq!(step1.status, StepStatus::Ok);
    assert_eq!(
        step1.payload.as_deref(),
        Some(&[0x81, 0xA4, 0x72, 0x6F, 0x77, 0x73, 0x78][..]),
        "MessagePack payload round-trips as raw bytes"
    );

    // ...while the crashed step is the running, result-less shape recovery
    // must re-execute live.
    let step4 = j
        .lookup_step(&id, 4, &StepKey::new("api.store.internal#w1"))
        .unwrap()
        .expect("step 4 present");
    assert_eq!(step4.status, StepStatus::Running);
    assert!(step4.ended_at.is_none());
}

#[test]
fn dead_flow_is_terminal_and_keeps_its_error() {
    let (_dir, j) = open_fixture("dead-flow");
    let id = FlowId::new("01JZWY0A0000000000000003");

    let flow = j.get_flow(&id).unwrap().expect("dead flow present");
    assert_eq!(flow.status, FlowStatus::Dead);

    assert!(
        j.incomplete_flows(true).unwrap().is_empty(),
        "a dead flow is not offered for recovery"
    );

    // The poison step keeps its error for `keel trace`.
    let step2 = j
        .lookup_step(&id, 2, &StepKey::new("api.billing.internal#w7"))
        .unwrap()
        .expect("step 2 present");
    assert_eq!(step2.status, StepStatus::Error);
    assert_eq!(step2.attempt, 5);
    assert_eq!(step2.error_class, Some(ErrorClass::Http));
}
