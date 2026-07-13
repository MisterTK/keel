//! Engine ⇄ journal integration: the persistent cache scope (read/write-through
//! the journal's `cache` table) and discovery recording. These exercise the
//! wiring the shared conformance corpus can't — it runs a bare, un-journaled
//! `Engine::new()`. The load-bearing cases: a persistent-cache round-trip across
//! two engines sharing one journal file, and graceful degradation when the
//! journal itself is broken (the call must still succeed, with a warning).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use keel_core::FlowDescriptor as FlowIdentity;
use keel_core::{Engine, FlowManager};
use keel_core_api::{AttemptResult, ENVELOPE_VERSION, ErrorClass, Request};
use keel_journal::{
    CacheKey, Clock, DiscoveryStore, FlowDescriptor, FlowId, FlowStatus, Journal, ManualClock,
    NewFlow, ProcessId, SqliteJournal, StepKey, StepOutcome,
};
use serde_json::{Value, json};
use tempfile::TempDir;

/// A fixed epoch for the manual clocks, matching the journal crate's own tests.
const T0: i64 = 1_783_728_000_000;

/// A cacheable request: carries an `args_hash`, so cache layers engage.
fn cache_request(target: &str) -> Request {
    Request {
        v: ENVELOPE_VERSION,
        target: target.to_owned(),
        op: format!("GET {target}/item"),
        idempotent: true,
        args_hash: Some("args-1".to_owned()),
    }
}

/// A non-cacheable request (no `args_hash`) for the discovery/retry paths.
fn plain_request(target: &str) -> Request {
    Request {
        v: ENVELOPE_VERSION,
        target: target.to_owned(),
        op: format!("GET {target}/x"),
        idempotent: true,
        args_hash: None,
    }
}

fn persistent_policy(target: &str, ttl: &str) -> Value {
    json!({ "target": { target: { "cache": { "ttl": ttl, "scope": "persistent" } } } })
}

/// The persistent scope survives across process boundaries: a payload written
/// by one engine is served — as a hit, with no effect invocation — by a
/// separate engine opened on the same journal file.
#[tokio::test(start_paused = true)]
async fn persistent_cache_round_trips_across_engines_sharing_a_journal() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("journal.db");
    let clock = ManualClock::new(T0);
    let policy = persistent_policy("api.catalog.internal", "5m");
    let request = cache_request("api.catalog.internal");
    let cached = json!({ "sku": "A-1", "price": 42 });

    // Engine A: cold cache → one live attempt → write-through to the journal.
    let mut a = Engine::new();
    a.attach_journal(SqliteJournal::open(&path, clock.clone()).expect("open journal a"));
    a.configure(&policy).expect("valid policy");
    let out_a = a
        .execute(&request, {
            let cached = cached.clone();
            async move |_attempt| AttemptResult::Ok {
                payload: cached.clone(),
            }
        })
        .await;
    assert_eq!(out_a.result, "ok");
    assert!(!out_a.from_cache, "engine A must miss the cold cache");
    assert_eq!(out_a.attempts, 1);

    // Engine B: a separate engine on the SAME file → a hit with attempts 0 and
    // the effect never run (it returns a sentinel that must not surface).
    let mut b = Engine::new();
    b.attach_journal(SqliteJournal::open(&path, clock.clone()).expect("open journal b"));
    b.configure(&policy).expect("valid policy");
    let out_b = b
        .execute(&request, async |_attempt| AttemptResult::Ok {
            payload: json!("LIVE-CALL-SHOULD-NOT-RUN"),
        })
        .await;
    assert!(
        out_b.from_cache,
        "engine B must serve from the shared journal"
    );
    assert_eq!(out_b.attempts, 0);
    assert_eq!(out_b.payload, Some(cached));
}

/// TTL semantics match the in-memory scope: fresh within the window, a miss
/// after — measured against the journal's (here manual) clock.
#[tokio::test(start_paused = true)]
async fn persistent_cache_expires_against_the_journal_clock() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("journal.db");
    let clock = ManualClock::new(T0);
    let request = cache_request("api.catalog.internal");

    let mut engine = Engine::new();
    engine.attach_journal(SqliteJournal::open(&path, clock.clone()).unwrap());
    engine
        .configure(&persistent_policy("api.catalog.internal", "1m"))
        .unwrap();

    // Cold: a live call writes an entry good for 60s.
    let out1 = engine
        .execute(&request, async |_| AttemptResult::Ok {
            payload: json!("v1"),
        })
        .await;
    assert!(!out1.from_cache);
    assert_eq!(out1.attempts, 1);

    // Within TTL: a hit, effect not run.
    clock.advance(30_000);
    let out2 = engine
        .execute(&request, async |_| AttemptResult::Ok {
            payload: json!("SENTINEL"),
        })
        .await;
    assert!(out2.from_cache);
    assert_eq!(out2.attempts, 0);
    assert_eq!(out2.payload, Some(json!("v1")));

    // Past TTL: expired → miss → a fresh live call replaces the entry.
    clock.advance(31_000); // now T0 + 61s, past the 60s expiry
    let out3 = engine
        .execute(&request, async |_| AttemptResult::Ok {
            payload: json!("v2"),
        })
        .await;
    assert!(
        !out3.from_cache,
        "the entry must expire against the journal clock"
    );
    assert_eq!(out3.attempts, 1);
    assert_eq!(out3.payload, Some(json!("v2")));
}

/// Every `execute` records exactly one observation; repeated calls to one
/// target fold into a single accumulating row.
#[tokio::test(start_paused = true)]
async fn discovery_accumulates_one_row_per_call() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("discovery.db");
    let clock = ManualClock::new(T0);

    let mut engine = Engine::new();
    engine.attach_discovery(DiscoveryStore::open(&path, clock.clone()).unwrap());
    engine
        .configure(&json!({
            "target": { "api.x": {
                "retry": { "attempts": 3, "schedule": "fixed(10ms)", "on": ["conn"] }
            } }
        }))
        .unwrap();

    // A clean success on the first attempt.
    let ok = engine
        .execute(&plain_request("api.x"), async |_| AttemptResult::Ok {
            payload: json!("ok"),
        })
        .await;
    assert_eq!(ok.result, "ok");
    assert_eq!(ok.attempts, 1);

    // A conn failure that exhausts all three attempts (two retries).
    let fail = engine
        .execute(&plain_request("api.x"), async |_| AttemptResult::Error {
            class: ErrorClass::Conn,
            http_status: None,
            retry_after_ms: None,
            message: String::from("connection refused"),
            original: None,
        })
        .await;
    assert_eq!(fail.result, "error");
    assert_eq!(fail.attempts, 3);

    // A second handle on the same file sees both calls folded into one row.
    let reader = DiscoveryStore::open(&path, clock.clone()).unwrap();
    let snapshot = reader.snapshot().unwrap();
    assert_eq!(snapshot.len(), 1, "one target → one row");
    let stats = &snapshot[0];
    assert_eq!(stats.target, "api.x");
    assert_eq!(stats.calls, 2);
    assert_eq!(stats.successes, 1);
    assert_eq!(stats.failures, 1);
    assert_eq!(stats.attempts, 4, "1 (success) + 3 (exhausted)");
    assert_eq!(stats.retries, 2, "0 + 2");
    assert_eq!(stats.last_error_class, Some(ErrorClass::Conn));
}

/// Resilience first: a journal whose every operation fails must not fail, alter,
/// or stall the user's call — it degrades to a live call and a warning.
#[tokio::test(start_paused = true)]
async fn unwritable_journal_degrades_persistent_cache_to_live_calls() {
    let mut engine = Engine::new();
    engine.attach_journal(FailingJournal);
    engine
        .configure(&persistent_policy("api.catalog.internal", "5m"))
        .unwrap();

    let warns = Arc::new(AtomicUsize::new(0));
    let dispatch = tracing::Dispatch::new(WarnCounter(warns.clone()));
    let guard = tracing::dispatcher::set_default(&dispatch);

    let out = engine
        .execute(&cache_request("api.catalog.internal"), async |_| {
            AttemptResult::Ok {
                payload: json!({ "ok": true }),
            }
        })
        .await;

    drop(guard);

    // The broken journal never fails or alters the call.
    assert_eq!(out.result, "ok");
    assert!(!out.from_cache);
    assert_eq!(out.attempts, 1);
    assert_eq!(out.payload, Some(json!({ "ok": true })));
    // Both the failed read-through and the failed write-through emit a warning.
    assert!(
        warns.load(Ordering::SeqCst) >= 2,
        "expected read + write degradation warnings, saw {}",
        warns.load(Ordering::SeqCst)
    );
}

// ---- policy `journal` backend selection (architecture spec §4.2) ----

/// `journal = "file:<path>"` is honored at configure time: the SQLite store is
/// attached at the custom path (parent directories created), and persistent
/// cache entries land there — provable by a second engine configured with the
/// same policy serving the first engine's write.
#[tokio::test(start_paused = true)]
async fn policy_file_journal_is_attached_at_the_custom_path() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("custom").join("nested").join("j.db");
    let mut policy = persistent_policy("api.catalog.internal", "5m");
    policy["journal"] = json!(format!("file:{}", path.display()));

    let a = Engine::new();
    a.configure(&policy).expect("file: journal is honored");
    assert!(a.journal().is_some(), "configure attached the journal");
    assert!(
        path.exists(),
        "store created at the policy path, directories included"
    );

    let out_a = a
        .execute(&cache_request("api.catalog.internal"), async |_| {
            AttemptResult::Ok {
                payload: json!("v1"),
            }
        })
        .await;
    assert!(!out_a.from_cache, "cold cache on the fresh custom store");

    let b = Engine::new();
    b.configure(&policy)
        .expect("same policy on a second engine");
    let out_b = b
        .execute(&cache_request("api.catalog.internal"), async |_| {
            AttemptResult::Ok {
                payload: json!("SENTINEL-NOT-SERVED"),
            }
        })
        .await;
    assert!(
        out_b.from_cache,
        "the entry round-trips through the custom path"
    );
    assert_eq!(out_b.payload, Some(json!("v1")));
}

/// Tier 2 flows land in the policy-selected journal: a manager built over
/// `engine.journal()` *after* configure (the bindings' pattern) writes its flow
/// rows into the custom file.
#[tokio::test(start_paused = true)]
async fn flows_land_in_the_policy_selected_journal() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("flows").join("j.db");
    let engine = Arc::new(Engine::new());
    engine
        .configure(&json!({ "journal": format!("file:{}", path.display()) }))
        .unwrap();

    let journal = engine.journal().expect("policy attached a journal");
    let clock: Arc<dyn Clock> = Arc::new(ManualClock::new(T0));
    let manager = FlowManager::new(
        Arc::clone(&engine),
        Arc::clone(&journal),
        clock,
        ProcessId::new("host-a:pid-1"),
    );
    let mut handle = manager
        .enter_flow(&FlowIdentity {
            entrypoint: "py:pipeline.ingest:main".to_owned(),
            args_hash: "a1".to_owned(),
            explicit_key: None,
            code_hash: Some("ch-1".to_owned()),
        })
        .unwrap();
    let fid = handle.flow_id().clone();
    let out = handle
        .execute_step(&cache_request("api.step.internal"), |_attempt: u32| async {
            AttemptResult::Ok {
                payload: json!({ "rows": 1 }),
            }
        })
        .await;
    assert_eq!(out.result, "ok");
    handle.complete_success();
    drop(handle);

    // The flow row lives in the CUSTOM file, read through a fresh handle.
    let reader = SqliteJournal::open(&path, ManualClock::new(T0)).unwrap();
    let flow = reader
        .get_flow(&fid)
        .unwrap()
        .expect("flow journaled at the policy path");
    assert_eq!(flow.status, FlowStatus::Completed);
}

/// `journal = "postgres://…"` selects the real Postgres/fleet backend
/// (architecture spec §6); a malformed one still fails configure loudly
/// (KEEL-E040, same taxonomy slot an unopenable `file:` path uses), the URL
/// (which can carry credentials) never enters the diagnostic, and the
/// previous configuration stays fully in force. This exercises only the
/// failure path with a URL chosen to fail fast and offline (a real connection
/// against a live cluster is `crates/keel-journal`'s integration coverage,
/// and Tier 2 conformance over `PostgresJournal` is
/// `tests/flows_conformance_postgres.rs`).
#[tokio::test(start_paused = true)]
async fn malformed_postgres_journal_fails_configure_with_e040() {
    let engine = Engine::new();
    engine
        .configure(&persistent_policy("api.catalog.internal", "5m"))
        .unwrap();

    let err = engine
        .configure(&json!({ "journal": "postgres://keel:sekrit@[not-a-valid-host/keel" }))
        .expect_err("a malformed postgres:// location cannot be opened");
    assert_eq!(err.code.as_str(), "KEEL-E040");
    assert!(
        !err.message.contains("sekrit"),
        "credentials never enter the diagnostic"
    );
    assert!(
        engine.journal().is_none(),
        "the rejected location attaches nothing"
    );

    // The previous policy still governs: its cache layer still serves.
    let out1 = engine
        .execute(&cache_request("api.catalog.internal"), async |_| {
            AttemptResult::Ok { payload: json!(1) }
        })
        .await;
    let out2 = engine
        .execute(&cache_request("api.catalog.internal"), async |_| {
            AttemptResult::Ok { payload: json!(2) }
        })
        .await;
    assert!(!out1.from_cache);
    assert!(
        out2.from_cache,
        "previous policy still in force after the rejected configure"
    );
}

/// No `journal` key: the construction-time attachment is untouched — the
/// default behavior is unchanged by the policy wiring.
#[tokio::test(start_paused = true)]
async fn absent_journal_key_keeps_the_construction_attachment() {
    let dir = TempDir::new().unwrap();
    let mut engine = Engine::new();
    engine.attach_journal(
        SqliteJournal::open(dir.path().join("journal.db"), ManualClock::new(T0)).unwrap(),
    );
    let before = engine.journal().expect("attached at construction");
    engine
        .configure(&persistent_policy("api.catalog.internal", "5m"))
        .unwrap();
    let after = engine.journal().expect("still attached");
    assert!(
        Arc::ptr_eq(&before, &after),
        "no journal key leaves the attachment untouched"
    );
}

/// A policy `file:` location replaces the construction-time attachment (the
/// effective policy is authoritative), and reapplying the same location is a
/// no-op — the open store is kept, not re-opened.
#[tokio::test(start_paused = true)]
async fn policy_journal_replaces_construction_attachment_and_reapplies_idempotently() {
    let dir = TempDir::new().unwrap();
    let mut engine = Engine::new();
    engine.attach_journal(
        SqliteJournal::open(dir.path().join("default.db"), ManualClock::new(T0)).unwrap(),
    );
    let constructed = engine.journal().unwrap();

    let custom = dir.path().join("custom.db");
    let mut policy = persistent_policy("api.catalog.internal", "5m");
    policy["journal"] = json!(format!("file:{}", custom.display()));
    engine.configure(&policy).unwrap();
    let selected = engine.journal().unwrap();
    assert!(
        !Arc::ptr_eq(&constructed, &selected),
        "policy file: replaces the construction journal"
    );
    assert!(custom.exists());

    engine.configure(&policy).unwrap();
    let reapplied = engine.journal().unwrap();
    assert!(
        Arc::ptr_eq(&selected, &reapplied),
        "an unchanged location keeps the open store"
    );
}

/// A `Journal` whose every operation fails — a poisoned/unwritable journal, so
/// the engine's degradation paths can be exercised deterministically.
#[derive(Debug)]
struct FailingJournal;

fn injected_failure() -> keel_journal::Error {
    keel_journal::Error::Corrupt {
        column: "failing-journal",
        value: "injected failure".to_owned(),
    }
}

impl Journal for FailingJournal {
    fn begin_flow(&self, _flow: &NewFlow) -> keel_journal::Result<FlowId> {
        Err(injected_failure())
    }
    fn record_step(
        &self,
        _flow: &FlowId,
        _seq: u64,
        _key: &StepKey,
        _outcome: &StepOutcome,
    ) -> keel_journal::Result<()> {
        Err(injected_failure())
    }
    fn lookup_step(
        &self,
        _flow: &FlowId,
        _seq: u64,
        _key: &StepKey,
    ) -> keel_journal::Result<Option<StepOutcome>> {
        Err(injected_failure())
    }
    fn step_at(
        &self,
        _flow: &FlowId,
        _seq: u64,
    ) -> keel_journal::Result<Option<(StepKey, StepOutcome)>> {
        Err(injected_failure())
    }
    fn get_flow(&self, _flow: &FlowId) -> keel_journal::Result<Option<FlowDescriptor>> {
        Err(injected_failure())
    }
    fn complete_flow(&self, _flow: &FlowId, _status: FlowStatus) -> keel_journal::Result<()> {
        Err(injected_failure())
    }
    fn incomplete_flows(&self, _lease_expired: bool) -> keel_journal::Result<Vec<FlowDescriptor>> {
        Err(injected_failure())
    }
    fn acquire_lease(
        &self,
        _flow: &FlowId,
        _holder: &ProcessId,
        _ttl: Duration,
    ) -> keel_journal::Result<bool> {
        Err(injected_failure())
    }
    fn put_cache(
        &self,
        _key: &CacheKey,
        _value: &[u8],
        _ttl: Duration,
    ) -> keel_journal::Result<()> {
        Err(injected_failure())
    }
    fn get_cache(&self, _key: &CacheKey) -> keel_journal::Result<Option<Vec<u8>>> {
        Err(injected_failure())
    }
}

/// A minimal tracing subscriber that counts WARN-level events — enough to prove
/// the degradation paths actually warn, without pulling in tracing-subscriber.
#[derive(Debug)]
struct WarnCounter(Arc<AtomicUsize>);

impl tracing::Subscriber for WarnCounter {
    fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        if *event.metadata().level() == tracing::Level::WARN {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    fn enter(&self, _span: &tracing::span::Id) {}
    fn exit(&self, _span: &tracing::span::Id) {}
}
