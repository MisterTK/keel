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
//! actually ran live (a replayed step must not call its effect). An effect
//! step's `idempotency_key` (contracts/adapter-pack.md "Idempotency-key
//! injection") drives `execute_step_with_idempotency_key`; `expect_recorded_key`
//! peeks `recorded_idempotency_key` before the step runs and compares it to a
//! JSON string or `null`.
//!
//! A handful of `RunSpec`/`StepSpec` fields beyond that baseline (`holder`,
//! `hold`, `expect_enter_error`, `advance_before_ms`, a run-scoped `policy` or
//! `code_hash` override, `inject_running`, `expect_journal`, and a value-step
//! `kind`) exist purely so this JSON-driven interpreter can express the
//! lease/clock/policy-change scenarios `conformance/README.md`'s Tier 2
//! section documents but scenarios 16-17 alone could not exercise. See
//! `conformance/README.md`'s "Extended run/step fields" note for the format.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use keel_conformance::{scenarios_dir, subset_mismatches};
use keel_core_api::{AttemptResult, ENVELOPE_VERSION, KeelError, Request};
use keel_journal::{
    Clock, FlowId, Journal, ManualClock, ProcessId, SqliteJournal, StepKey, StepKind, StepOutcome,
    StepStatus,
};
use keelrun_core::{Engine, FlowConfig, FlowDescriptor, FlowHandle, FlowManager};
use serde::Deserialize;
use serde_json::Value;
use tempfile::TempDir;

const T0: i64 = 1_783_728_000_000;
/// Past the default 30s lease TTL — the standard "let the lease lapse" jump
/// used after a `crash` run end.
const PAST_LEASE_TTL_MS: i64 = 31_000;

#[derive(Debug, Deserialize)]
struct FlowScenario {
    name: String,
    policy: Value,
    /// Overrides `FlowConfig::max_attempts` for every manager in this
    /// scenario (default 3, matching `FlowConfig::default()`).
    #[serde(default)]
    max_attempts: Option<u32>,
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

fn default_holder() -> String {
    String::from("host-a:pid-1")
}

#[derive(Debug, Deserialize)]
struct RunSpec {
    /// Which process id enters this run — lets a scenario model a second
    /// holder racing/succeeding a lease (scenario 21).
    #[serde(default = "default_holder")]
    holder: String,
    /// Clock advance applied *before* this run enters, in addition to the
    /// `crash` end's own advance — e.g. to let a held lease expire.
    #[serde(default)]
    advance_before_ms: i64,
    /// Reconfigures the (shared) engine before this run enters, so a later
    /// run can prove a replayed step ignores the policy in effect when it
    /// re-executes live (scenario 27).
    #[serde(default)]
    policy: Option<Value>,
    /// Overrides the flow descriptor's `code_hash` for this run's entry only
    /// (the identity's `code_hash` is recorded once, at first entry; a later
    /// override only affects the current-vs-recorded fencing comparison).
    #[serde(default)]
    code_hash: Option<String>,
    /// Keep the handle open past this run instead of completing/crashing it —
    /// models a still-live holder for a lease-contention scenario.
    #[serde(default)]
    hold: bool,
    /// When set, this run's `enter_flow` must fail; subset-matched against
    /// `{"code": "...", "message": "..."}`. No steps run in this case.
    #[serde(default)]
    expect_enter_error: Option<Value>,
    #[serde(default)]
    end: RunEnd,
    #[serde(default)]
    expect_effect_calls: Option<usize>,
    #[serde(default)]
    steps: Vec<StepSpec>,
    /// After this run's `steps`, directly journal a `running` (unterminated)
    /// record at `seq` — simulating a crash mid-effect, which a live step's
    /// completed-and-recorded terminal outcome cannot express.
    #[serde(default)]
    inject_running: Option<InjectRunning>,
    /// Subset assertions against the raw journal, checked after `steps` (and
    /// `inject_running`) but before `end` — e.g. to pin exactly where a
    /// branch marker landed.
    #[serde(default)]
    expect_journal: Vec<JournalAssertion>,
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
struct InjectRunning {
    seq: u64,
    target: String,
    #[serde(default)]
    args_hash: Option<String>,
}

#[derive(Debug, Deserialize)]
struct JournalAssertion {
    seq: u64,
    key: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    kind: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum StepSpecKind {
    #[default]
    Effect,
    Time,
    Random,
}

#[derive(Debug, Deserialize)]
struct StepSpec {
    #[serde(default)]
    kind: StepSpecKind,
    /// Effect steps only: the request target.
    #[serde(default)]
    target: Option<String>,
    /// Effect steps only: the request args_hash.
    #[serde(default)]
    args_hash: Option<String>,
    /// Effect steps only: an idempotency key the "adapter" minted/injected for
    /// this step (contracts/adapter-pack.md "Idempotency-key injection" rule
    /// 3); when set, drives `execute_step_with_idempotency_key` instead of the
    /// plain `execute_step` (identical when `None` — the former IS the
    /// latter's implementation).
    #[serde(default)]
    idempotency_key: Option<String>,
    /// Effect steps only: when present, PEEKED via `recorded_idempotency_key`
    /// before this step executes and compared to this value (a JSON string,
    /// or `null` for "no key recorded") — the resume-reuse read a real
    /// adapter performs before minting/injecting (rule 3).
    #[serde(default)]
    expect_recorded_key: Option<Value>,
    /// Value (`time`/`random`) steps only: the explicit journaled key, e.g.
    /// `"py:time.time#-"`.
    #[serde(default)]
    key: Option<String>,
    /// The scripted result of an effect step's single attempt.
    #[serde(default)]
    effect: Option<AttemptResult>,
    /// Value steps only: the live `now_ms` (time) or byte array (random) the
    /// harness feeds in when the step is NOT a replay hit.
    #[serde(default)]
    live_value: Option<Value>,
    /// Subset-matched against an effect step's `Outcome`.
    #[serde(default)]
    expect: Value,
    /// Subset-matched against a value step's returned time/bytes.
    #[serde(default)]
    expect_value: Option<Value>,
}

/// Everything a scenario run shares across the flow identities it drives:
/// one engine (reconfigurable between runs), one journal, one clock, and a
/// manager per holder so a scenario can model more than one process.
struct ScenarioRig {
    engine: Arc<Engine>,
    journal: Arc<dyn Journal>,
    clock: ManualClock,
    clock_dyn: Arc<dyn Clock>,
    config: FlowConfig,
    managers: HashMap<String, FlowManager>,
    /// Handles kept open past their run (`hold: true`) — e.g. a live lease
    /// holder a later run must contend with.
    held: HashMap<String, FlowHandle>,
}

impl ScenarioRig {
    fn manager(&mut self, holder: &str) -> &FlowManager {
        self.managers.entry(holder.to_owned()).or_insert_with(|| {
            FlowManager::with_config(
                Arc::clone(&self.engine),
                Arc::clone(&self.journal),
                Arc::clone(&self.clock_dyn),
                ProcessId::new(holder),
                self.config,
            )
        })
    }
}

fn request(target: &str, args_hash: Option<&str>) -> Request {
    Request {
        v: ENVELOPE_VERSION,
        target: target.to_owned(),
        op: target.to_owned(),
        idempotent: true,
        args_hash: args_hash.map(str::to_owned),
    }
}

/// Run one effect step: peek `expect_recorded_key` (if set), then submit it
/// through `execute_step_with_idempotency_key` (a plain `execute_step` when
/// `idempotency_key` is `None` — the former IS the latter's implementation)
/// and subset-match the resulting `Outcome`.
async fn run_effect_step(
    handle: &mut FlowHandle,
    step: &StepSpec,
    calls: &Arc<AtomicUsize>,
    failures: &mut Vec<String>,
    ri: usize,
    si: usize,
) {
    let target = step
        .target
        .as_deref()
        .unwrap_or_else(|| panic!("run[{ri}] step[{si}]: an effect step needs `target`"));
    if let Some(expected) = &step.expect_recorded_key {
        let step_key = format!("{target}#{}", step.args_hash.as_deref().unwrap_or("-"));
        let actual = handle
            .recorded_idempotency_key(&step_key)
            .map_or(Value::Null, Value::String);
        if &actual != expected {
            failures.push(format!(
                "run[{ri}] step[{si}] recorded_idempotency_key({step_key:?}): expected {expected}, got {actual}"
            ));
        }
    }
    let effect = step
        .effect
        .clone()
        .unwrap_or_else(|| panic!("run[{ri}] step[{si}]: an effect step needs `effect`"));
    let calls_effect = Arc::clone(calls);
    let outcome = handle
        .execute_step_with_idempotency_key(
            &request(target, step.args_hash.as_deref()),
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

/// Run one virtualized value step (`time`/`random`): call the matching
/// `FlowHandle` method and subset-match the returned value against
/// `expect_value`.
fn run_value_step(
    handle: &mut FlowHandle,
    step: &StepSpec,
    failures: &mut Vec<String>,
    ri: usize,
    si: usize,
) {
    let key = step
        .key
        .as_deref()
        .unwrap_or_else(|| panic!("run[{ri}] step[{si}]: a value step needs `key`"));
    let result = if step.kind == StepSpecKind::Time {
        let now_ms = step
            .live_value
            .as_ref()
            .and_then(Value::as_i64)
            .unwrap_or_else(|| panic!("run[{ri}] step[{si}]: a time step needs int `live_value`"));
        handle
            .journal_time(key, now_ms)
            .map(|v| serde_json::to_value(v).expect("i64 serializes"))
    } else {
        let bytes: Vec<u8> = step
            .live_value
            .as_ref()
            .and_then(Value::as_array)
            .unwrap_or_else(|| {
                panic!("run[{ri}] step[{si}]: a random step needs array `live_value`")
            })
            .iter()
            .map(|b| {
                u8::try_from(b.as_u64().expect("byte value is a u64")).expect("byte value fits u8")
            })
            .collect();
        handle
            .journal_random(key, bytes)
            .map(|v| serde_json::to_value(v).expect("bytes serialize"))
    };
    match result {
        Ok(actual) => {
            if let Some(expected) = &step.expect_value {
                let mut mismatches = Vec::new();
                subset_mismatches(&actual, expected, "$", &mut mismatches);
                failures.extend(
                    mismatches
                        .into_iter()
                        .map(|m| format!("run[{ri}] step[{si}] value: {m}")),
                );
            }
        }
        Err(e) => failures.push(format!("run[{ri}] step[{si}]: value step failed: {e}")),
    }
}

/// Directly journal a `running` (unterminated) record — a crash mid-effect,
/// which no `execute_step` call (which always records a terminal outcome
/// before returning) can produce.
fn inject_running_step(rig: &ScenarioRig, fid: &FlowId, inj: &InjectRunning) {
    let key = StepKey::new(format!(
        "{}#{}",
        inj.target,
        inj.args_hash.as_deref().unwrap_or("-")
    ));
    let now = rig.clock.now_ms();
    let outcome = StepOutcome {
        kind: StepKind::Effect,
        attempt: 0,
        status: StepStatus::Running,
        payload: None,
        error_class: None,
        started_at: now,
        ended_at: None,
    };
    rig.journal
        .record_step(fid, inj.seq, &key, &outcome)
        .expect("inject_running record_step succeeds");
}

/// Subset-check a raw journal record against an `expect_journal` assertion —
/// e.g. that a branch marker landed at the expected seq/key.
fn check_journal_assertion(
    rig: &ScenarioRig,
    fid: &FlowId,
    assertion: &JournalAssertion,
    ri: usize,
    failures: &mut Vec<String>,
) {
    let seq = assertion.seq;
    match rig.journal.step_at(fid, seq) {
        Ok(Some((key, outcome))) => {
            if key.as_str() != assertion.key {
                failures.push(format!(
                    "run[{ri}] expect_journal seq {seq}: key {key} != {}",
                    assertion.key
                ));
            }
            if let Some(status) = &assertion.status
                && outcome.status.as_str() != status
            {
                failures.push(format!(
                    "run[{ri}] expect_journal seq {seq}: status {} != {status}",
                    outcome.status.as_str()
                ));
            }
            if let Some(kind) = &assertion.kind
                && outcome.kind.as_str() != kind
            {
                failures.push(format!(
                    "run[{ri}] expect_journal seq {seq}: kind {} != {kind}",
                    outcome.kind.as_str()
                ));
            }
        }
        Ok(None) => failures.push(format!(
            "run[{ri}] expect_journal seq {seq}: no record present"
        )),
        Err(e) => failures.push(format!(
            "run[{ri}] expect_journal seq {seq}: journal read failed: {e}"
        )),
    }
}

/// Enter this run's flow and, if `expect_enter_error` is set, check the
/// failure and report whether the caller should keep going (there is no
/// handle to run steps on either way).
fn handle_expected_enter_error(
    expected: &Value,
    enter_result: Result<FlowHandle, KeelError>,
    ri: usize,
    failures: &mut Vec<String>,
) {
    match enter_result {
        Err(e) => {
            let actual = serde_json::to_value(&e).expect("KeelError serializes");
            let mut mismatches = Vec::new();
            subset_mismatches(&actual, expected, "$", &mut mismatches);
            failures.extend(
                mismatches
                    .into_iter()
                    .map(|m| format!("run[{ri}] enter error: {m}")),
            );
        }
        Ok(_) => failures.push(format!(
            "run[{ri}]: expected enter_flow to fail, but it succeeded"
        )),
    }
}

async fn run_flow_scenario(scn: &FlowScenario) -> Vec<String> {
    let dir = TempDir::new().unwrap();
    let clock = ManualClock::new(T0);
    let journal: Arc<dyn Journal> =
        Arc::new(SqliteJournal::open(dir.path().join("journal.db"), clock.clone()).unwrap());
    let engine = Arc::new(Engine::new());
    if let Err(e) = engine.configure(&scn.policy) {
        return vec![format!("configure: unexpected error {e}")];
    }
    let clock_dyn: Arc<dyn Clock> = Arc::new(clock.clone());
    let mut rig = ScenarioRig {
        engine,
        journal,
        clock,
        clock_dyn,
        config: FlowConfig {
            lease_ttl: Duration::from_secs(30),
            max_attempts: scn.max_attempts.unwrap_or(3),
        },
        managers: HashMap::new(),
        held: HashMap::new(),
    };

    let mut failures = Vec::new();
    for (ri, run) in scn.runs.iter().enumerate() {
        // Each run reports its own mismatches into `failures`; a hard failure
        // to even enter the flow (not an `expect_enter_error` scenario) halts
        // the remaining runs, since there is nothing left to resume from.
        if !run_one(&mut rig, scn, ri, run, &mut failures).await {
            break;
        }
    }
    failures
}

/// Run one `runs[i]` entry against the shared rig, appending every mismatch
/// (outcome, journal, or enter-error subset match) to `failures`. Returns
/// `false` only on a genuine, unexpected `enter_flow` failure — the signal
/// that the scenario cannot continue past this run.
async fn run_one(
    rig: &mut ScenarioRig,
    scn: &FlowScenario,
    ri: usize,
    run: &RunSpec,
    failures: &mut Vec<String>,
) -> bool {
    if run.advance_before_ms != 0 {
        rig.clock.advance(run.advance_before_ms);
    }
    if let Some(policy) = &run.policy
        && let Err(e) = rig.engine.configure(policy)
    {
        failures.push(format!("run[{ri}]: policy reconfigure failed: {e}"));
        return false;
    }

    let desc = FlowDescriptor {
        entrypoint: scn.flow.entrypoint.clone(),
        args_hash: scn.flow.args_hash.clone(),
        explicit_key: None,
        code_hash: run.code_hash.clone().or_else(|| scn.flow.code_hash.clone()),
    };
    let enter_result = rig.manager(&run.holder).enter_flow(&desc);

    if let Some(expected) = &run.expect_enter_error {
        handle_expected_enter_error(expected, enter_result, ri, failures);
        return true;
    }

    let mut handle = match enter_result {
        Ok(handle) => handle,
        Err(e) => {
            failures.push(format!("run[{ri}]: enter failed: {e}"));
            return false;
        }
    };
    let fid = handle.flow_id().clone();
    let calls = Arc::new(AtomicUsize::new(0));
    for (si, step) in run.steps.iter().enumerate() {
        match step.kind {
            StepSpecKind::Effect => {
                run_effect_step(&mut handle, step, &calls, failures, ri, si).await;
            }
            StepSpecKind::Time | StepSpecKind::Random => {
                run_value_step(&mut handle, step, failures, ri, si);
            }
        }
    }
    if let Some(expected) = run.expect_effect_calls {
        let got = calls.load(Ordering::SeqCst);
        if got != expected {
            failures.push(format!(
                "run[{ri}]: expected {expected} live effect call(s), got {got}"
            ));
        }
    }
    if let Some(inj) = &run.inject_running {
        inject_running_step(rig, &fid, inj);
    }
    for assertion in &run.expect_journal {
        check_journal_assertion(rig, &fid, assertion, ri, failures);
    }

    if run.hold {
        rig.held.insert(run.holder.clone(), handle);
    } else {
        match run.end {
            RunEnd::Success => handle.complete_success(),
            RunEnd::Failed => handle.complete_failed(),
            RunEnd::Crash => {
                drop(handle);
                rig.clock.advance(PAST_LEASE_TTL_MS);
            }
        }
    }
    true
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
        ran >= 13,
        "expected the tier-2 flow scenarios to run, ran {ran}"
    );
    assert!(failed.is_empty(), "tier-2 scenarios failed: {failed:?}");
}
