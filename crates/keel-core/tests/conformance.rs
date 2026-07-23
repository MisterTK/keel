//! The real core against the SAME conformance corpus as the stubs
//! (conformance/scenarios/). Runs under tokio's paused test clock: the
//! engine's real `sleep`s advance virtual time deterministically, and
//! `advance_ms` steps map to `tokio::time::advance`.

use std::time::Duration;

use keel_conformance::{
    Scenario, ScriptedEffect, Step, load_dir, scenarios_dir, subset_mismatches,
};
use keelrun_core::Engine;

async fn run_scenario(scenario: &Scenario) -> Vec<String> {
    let engine = Engine::new();
    match (
        engine.configure(&scenario.policy),
        scenario.expect_configure_error.as_deref(),
    ) {
        (Ok(()), None) => {}
        (Ok(()), Some(expected)) => {
            return vec![format!(
                "configure: expected {expected}, but configure succeeded"
            )];
        }
        (Err(e), Some(expected)) => {
            if e.code.as_str() == expected {
                return vec![];
            }
            return vec![format!("configure: expected {expected}, got {}", e.code)];
        }
        (Err(e), None) => return vec![format!("configure: unexpected error {e}")],
    }

    let mut failures = Vec::new();
    for (i, step) in scenario.steps.iter().enumerate() {
        let label = format!("step[{i}]");
        match step {
            Step::Advance { advance_ms } => {
                tokio::time::advance(Duration::from_millis(*advance_ms)).await;
            }
            Step::ReportExpect { report_expect } => {
                let mut mismatches = Vec::new();
                subset_mismatches(&engine.report(), report_expect, "$", &mut mismatches);
                failures.extend(
                    mismatches
                        .into_iter()
                        .map(|m| format!("{label} report: {m}")),
                );
            }
            Step::Call { call } => {
                let request = call.request();
                let mut scripted = ScriptedEffect::new(label.clone(), &call.effect);
                let outcome = engine
                    .execute(&request, async |attempt| scripted.next(attempt))
                    .await;
                if let Some(leftover) = scripted.leftover() {
                    failures.push(leftover);
                }
                let actual = serde_json::to_value(&outcome).expect("outcome serializes");
                let mut mismatches = Vec::new();
                subset_mismatches(&actual, &call.expect, "$", &mut mismatches);
                failures.extend(
                    mismatches
                        .into_iter()
                        .map(|m| format!("{label} outcome: {m}")),
                );
            }
            Step::Resolve { resolve, expect } => {
                let got = engine.resolve_target(
                    &resolve.method,
                    &resolve.host,
                    resolve.scheme.as_deref(),
                    resolve.port,
                    resolve.path.as_deref(),
                );
                if got != *expect {
                    failures.push(format!("{label} resolve: got {got:?}, want {expect:?}"));
                }
            }
            Step::Layer { layer, expect } => {
                let got = engine.layer(&layer.target, &layer.key);
                if got != *expect {
                    failures.push(format!(
                        "{label} layer({},{}): got {got}, want {expect}",
                        layer.target, layer.key
                    ));
                }
            }
        }
    }
    failures
}

#[tokio::test(start_paused = true)]
async fn conformance() {
    let scenarios = load_dir(&scenarios_dir(env!("CARGO_MANIFEST_DIR")));
    let mut failed = Vec::new();
    for (_path, scenario) in &scenarios {
        if scenario.tier != 1 {
            // Tier 2 (durable flows) is exercised by tests/flows_conformance.rs;
            // this Tier 1 harness has no flow-step model, so it skips cleanly.
            println!("skip  {} (tier {})", scenario.name, scenario.tier);
            continue;
        }
        let mismatches = run_scenario(scenario).await;
        if mismatches.is_empty() {
            println!("ok    {}", scenario.name);
        } else {
            println!("FAIL  {}", scenario.name);
            for mismatch in &mismatches {
                println!("      {mismatch}");
            }
            failed.push(scenario.name.clone());
        }
    }
    assert!(
        failed.is_empty(),
        "{}/{} scenarios failed: {failed:?}",
        failed.len(),
        scenarios.len()
    );
}
