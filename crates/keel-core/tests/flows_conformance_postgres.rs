//! Tier-2 conformance (the baseline-shaped scenarios in
//! `conformance/scenarios/` — currently 16, 17, 24, 25, and 26; see
//! [`needs_extended_shape`] for what disqualifies the rest) driven against
//! the real core's [`FlowManager`] over a real [`PostgresJournal`],
//! not [`SqliteJournal`](keel_journal::SqliteJournal) — see
//! `tests/flows_conformance.rs` for that leg, which this file deliberately
//! does **not** parametrize alongside.
//!
//! # Why a dedicated file instead of parametrizing `flows_conformance.rs`
//!
//! `FlowManager`/`Engine` are already fully generic over `Arc<dyn Journal>`,
//! so most of the harness *would* lift cleanly. The one piece that does not:
//! `SqliteJournal` is generic over an injected [`Clock`], so the existing
//! harness runs under `#[tokio::test(start_paused = true)]` and simulates a
//! crash's lease expiry by calling `clock.advance(31_000)` — an instant,
//! virtual jump that also happens to be the SQLite journal's own notion of
//! "now" (same `ManualClock` instance). `PostgresJournal` deliberately has
//! **no** injectable clock at all (see its module doc: a fleet's lease
//! arbitration must be judged against the *server's* real-time clock, not any
//! one process's) — so there is nothing to advance. Simulating a crash here
//! means using a short *real* lease TTL and a short *real* sleep past it, and
//! running the async steps under plain (unpaused) Tokio time instead. That is
//! a genuinely different mechanism, not just a different journal constructor,
//! so it gets its own file rather than a branch in the shared one — this
//! keeps `flows_conformance.rs` untouched and its virtual-time guarantees
//! (no real sleeps) intact for the backend that can honor them.
//!
//! Everything else — scenario shape, step execution, subset-matching against
//! `expect` — is copied from `flows_conformance.rs` verbatim; see that file
//! for the format doc.
//!
//! This file's `FlowScenario`/`RunSpec`/`StepSpec` are frozen at that
//! baseline shape and deliberately do NOT grow alongside
//! `flows_conformance.rs`'s later extended fields (`holder`/`hold`, run-scoped
//! `policy`/`code_hash` overrides, `expect_enter_error`, `inject_running`,
//! `expect_journal`, a value-step `kind`): several of those exist to model
//! things (a second real holder, a 30s-TTL-scaled clock jump) that don't fit
//! this file's real-clock, single-holder, short-lease design at all. Rather
//! than hand-panic on the resulting shape mismatch, [`needs_extended_shape`]
//! detects it and this file skips those scenarios — they are exercised in
//! full by the SQLite leg (`flows_conformance.rs`), and this file keeps
//! covering the baseline semantics (crash/resume, nondeterminism-fail,
//! idempotency-key resume-reuse) against a REAL `PostgresJournal`.

mod support;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use keel_conformance::{scenarios_dir, subset_mismatches};
use keel_core_api::{AttemptResult, ENVELOPE_VERSION, Request};
use keel_journal::{Clock, Journal, ManualClock, PostgresJournal, ProcessId};
use keelrun_core::{Engine, FlowConfig, FlowDescriptor, FlowManager};
use serde::Deserialize;
use serde_json::Value;
use support::ScratchPg;

const T0: i64 = 1_783_728_000_000;

/// Real time a "crash" run-end waits before the next run, chosen to clear
/// [`LEASE_TTL`] comfortably without dragging out the test.
const CRASH_WAIT: Duration = Duration::from_millis(500);
/// A short real lease TTL so [`CRASH_WAIT`] can be short too — this is a test
/// tuning knob, not a semantic difference from the SQLite leg's (virtual)
/// 30s default.
const LEASE_TTL: Duration = Duration::from_millis(150);

#[derive(Debug, Deserialize)]
struct FlowScenario {
    name: String,
    policy: Value,
    flow: FlowSpec,
    runs: Vec<RunSpec>,
}

#[derive(Debug, Deserialize)]
struct FlowSpec {
    entrypoint: String,
    args_hash: String,
    #[serde(default)]
    code_hash: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RunSpec {
    #[serde(default)]
    end: RunEnd,
    #[serde(default)]
    expect_effect_calls: Option<usize>,
    steps: Vec<StepSpec>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RunEnd {
    #[default]
    Success,
    Failed,
    /// Drop the handle mid-flight and let the lease expire — a crash.
    Crash,
}

#[derive(Debug, Deserialize)]
struct StepSpec {
    target: String,
    #[serde(default)]
    args_hash: Option<String>,
    #[serde(default)]
    idempotency_key: Option<String>,
    #[serde(default)]
    expect_recorded_key: Option<Value>,
    effect: AttemptResult,
    #[serde(default)]
    expect: Value,
}

/// Whether `scenario` (the raw JSON `Value`) uses a `RunSpec`/`StepSpec` field
/// this file's baseline structs don't have — either a run missing `steps`
/// (relying on the richer interpreter's default) or one of its extended keys,
/// or a step selecting a virtualized `kind` instead of an effect. See the
/// module doc for why this file skips these instead of implementing them.
fn needs_extended_shape(scenario: &Value) -> bool {
    const RUN_KEYS: [&str; 8] = [
        "holder",
        "advance_before_ms",
        "policy",
        "code_hash",
        "hold",
        "expect_enter_error",
        "inject_running",
        "expect_journal",
    ];
    scenario["runs"].as_array().is_some_and(|runs| {
        runs.iter().any(|run| {
            !run.get("steps").is_some_and(Value::is_array)
                || RUN_KEYS.iter().any(|k| run.get(*k).is_some())
                || run["steps"]
                    .as_array()
                    .is_some_and(|steps| steps.iter().any(|s| s.get("kind").is_some()))
        })
    })
}

async fn run_flow_scenario(scn: &FlowScenario, journal: Arc<dyn Journal>) -> Vec<String> {
    let clock = ManualClock::new(T0);
    let engine = Engine::new();
    if let Err(e) = engine.configure(&scn.policy) {
        return vec![format!("configure: unexpected error {e}")];
    }
    let clock_dyn: Arc<dyn Clock> = Arc::new(clock.clone());
    let manager = FlowManager::with_config(
        Arc::new(engine),
        Arc::clone(&journal),
        clock_dyn,
        ProcessId::new("host-conformance-pg:pid-1"),
        FlowConfig {
            lease_ttl: LEASE_TTL,
            max_attempts: 3,
        },
    );
    let desc = FlowDescriptor {
        entrypoint: scn.flow.entrypoint.clone(),
        args_hash: scn.flow.args_hash.clone(),
        explicit_key: None,
        code_hash: scn.flow.code_hash.clone(),
    };

    let mut failures = Vec::new();
    for (ri, run) in scn.runs.iter().enumerate() {
        let mut handle = match manager.enter_flow(&desc) {
            Ok(handle) => handle,
            Err(e) => {
                failures.push(format!("run[{ri}]: enter failed: {e}"));
                return failures;
            }
        };
        let calls = Arc::new(AtomicUsize::new(0));
        for (si, step) in run.steps.iter().enumerate() {
            if let Some(expected) = &step.expect_recorded_key {
                let step_key = format!(
                    "{}#{}",
                    step.target,
                    step.args_hash.as_deref().unwrap_or("-")
                );
                let actual = handle
                    .recorded_idempotency_key(&step_key)
                    .map_or(Value::Null, Value::String);
                if &actual != expected {
                    failures.push(format!(
                        "run[{ri}] step[{si}] recorded_idempotency_key({step_key:?}): expected {expected}, got {actual}"
                    ));
                }
            }
            let request = Request {
                v: ENVELOPE_VERSION,
                target: step.target.clone(),
                op: step.target.clone(),
                idempotent: true,
                args_hash: step.args_hash.clone(),
            };
            let effect = step.effect.clone();
            let calls_effect = Arc::clone(&calls);
            let outcome = handle
                .execute_step_with_idempotency_key(
                    &request,
                    step.idempotency_key.as_deref(),
                    move |_attempt: u32| {
                        let effect = effect.clone();
                        let calls_effect = Arc::clone(&calls_effect);
                        async move {
                            calls_effect.fetch_add(1, Ordering::SeqCst);
                            effect
                        }
                    },
                )
                .await;
            let actual = serde_json::to_value(&outcome).expect("outcome serializes");
            let mut mismatches = Vec::new();
            subset_mismatches(&actual, &step.expect, "$", &mut mismatches);
            failures.extend(
                mismatches
                    .into_iter()
                    .map(|m| format!("run[{ri}] step[{si}] outcome: {m}")),
            );
        }
        if let Some(expected) = run.expect_effect_calls {
            let got = calls.load(Ordering::SeqCst);
            if got != expected {
                failures.push(format!(
                    "run[{ri}]: expected {expected} live effect call(s), got {got}"
                ));
            }
        }
        match run.end {
            RunEnd::Success => handle.complete_success(),
            RunEnd::Failed => handle.complete_failed(),
            RunEnd::Crash => {
                drop(handle);
                // No virtual clock to advance for a Postgres-backed lease
                // (see module doc) — wait past the short real TTL instead.
                tokio::time::sleep(CRASH_WAIT).await;
            }
        }
    }
    failures
}

#[tokio::test]
async fn tier2_flow_conformance_over_postgres() {
    let Some(pg) = ScratchPg::start() else {
        eprintln!("skipping: no local `initdb` found (see tests/support)");
        return;
    };
    let journal: Arc<dyn Journal> =
        Arc::new(PostgresJournal::open(&pg.url()).expect("open postgres journal"));

    let dir = scenarios_dir(env!("CARGO_MANIFEST_DIR"));
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
        .map(|entry| entry.expect("dir entry").path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "json"))
        .collect();
    paths.sort();

    let mut ran = 0;
    let mut failed = Vec::new();
    for path in paths {
        let text = std::fs::read_to_string(&path).unwrap();
        let value: Value = serde_json::from_str(&text).expect("scenario is valid JSON");
        if value.get("tier").and_then(Value::as_u64) != Some(2) {
            continue; // Tier 1 scenarios are covered by conformance.rs.
        }
        if needs_extended_shape(&value) {
            // Exercised in full by the SQLite leg; see the module doc.
            println!(
                "skip  {} (needs the extended SQLite-leg shape)",
                path.display()
            );
            continue;
        }
        let scenario: FlowScenario = serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("bad tier-2 scenario {}: {e}", path.display()));
        let mismatches = run_flow_scenario(&scenario, Arc::clone(&journal)).await;
        if mismatches.is_empty() {
            println!("ok    {}", scenario.name);
            ran += 1;
        } else {
            println!("FAIL  {}", scenario.name);
            for m in &mismatches {
                println!("      {m}");
            }
            failed.push(scenario.name);
        }
    }

    assert!(
        ran >= 2,
        "expected the tier-2 flow scenarios to run, ran {ran}"
    );
    assert!(failed.is_empty(), "tier-2 scenarios failed: {failed:?}");
}
