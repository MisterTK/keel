//! Tier-2 conformance: the durable-flow scenarios (`conformance/scenarios/`
//! marked `"tier": 2`) driven against the real core's [`FlowManager`]. The Tier
//! 1 harnesses (Rust stub, real-core `conformance.rs`, ABI, plus the Python and
//! Node stubs) skip these — only the real core implements Tier 2 — so this is
//! their sole executor. Runs under tokio's paused clock on a `ManualClock`.
//!
//! Scenario shape (a superset the generic Tier 1 model ignores): `flow`
//! identity + a list of `runs`, each a sequence of `steps` ending in `crash`
//! (drop the handle, let the lease expire), `success`, or `failed`. A step's
//! `effect` scripts its single attempt; `expect` is subset-matched against the
//! resulting `Outcome`; `expect_effect_calls` asserts how many step effects
//! actually ran live (a replayed step must not call its effect). A step's
//! `idempotency_key` (contracts/adapter-pack.md "Idempotency-key injection")
//! drives `execute_step_with_idempotency_key`; `expect_recorded_key` peeks
//! `recorded_idempotency_key` before the step runs and compares it to a JSON
//! string or `null`.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use keel_conformance::{scenarios_dir, subset_mismatches};
use keel_core::{Engine, FlowDescriptor, FlowManager};
use keel_core_api::{AttemptResult, ENVELOPE_VERSION, Request};
use keel_journal::{Clock, Journal, ManualClock, ProcessId, SqliteJournal};
use serde::Deserialize;
use serde_json::Value;
use tempfile::TempDir;

const T0: i64 = 1_783_728_000_000;

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
    /// An idempotency key the "adapter" minted/injected for this step
    /// (contracts/adapter-pack.md "Idempotency-key injection" rule 3); when
    /// set, drives `execute_step_with_idempotency_key` instead of the
    /// plain `execute_step` (identical when `None` — the former IS the
    /// latter's implementation).
    #[serde(default)]
    idempotency_key: Option<String>,
    /// When present, PEEKED via `recorded_idempotency_key` before this step
    /// executes and compared to this value (a JSON string, or `null` for
    /// "no key recorded") — the resume-reuse read a real adapter performs
    /// before minting/injecting (rule 3).
    #[serde(default)]
    expect_recorded_key: Option<Value>,
    /// The scripted result of this step's single attempt.
    effect: AttemptResult,
    /// Subset-matched against the step's `Outcome`.
    #[serde(default)]
    expect: Value,
}

async fn run_flow_scenario(scn: &FlowScenario) -> Vec<String> {
    let dir = TempDir::new().unwrap();
    let clock = ManualClock::new(T0);
    let journal: Arc<dyn Journal> =
        Arc::new(SqliteJournal::open(dir.path().join("journal.db"), clock.clone()).unwrap());
    let engine = Engine::new();
    if let Err(e) = engine.configure(&scn.policy) {
        return vec![format!("configure: unexpected error {e}")];
    }
    let clock_dyn: Arc<dyn Clock> = Arc::new(clock.clone());
    let manager = FlowManager::new(
        Arc::new(engine),
        Arc::clone(&journal),
        clock_dyn,
        ProcessId::new("host-conformance:pid-1"),
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
                clock.advance(31_000); // past the default 30s lease
            }
        }
    }
    failures
}

#[tokio::test(start_paused = true)]
async fn tier2_flow_conformance() {
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
        let scenario: FlowScenario = serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("bad tier-2 scenario {}: {e}", path.display()));
        let mismatches = run_flow_scenario(&scenario).await;
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
