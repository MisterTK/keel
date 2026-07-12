//! Rust harness for the shared conformance suite (conformance/scenarios/).
//! The same scenarios run against the Python and Node stubs; the real core
//! must pass them too. Format and semantics: conformance/README.md.

use keel_core_api::{AttemptResult, ENVELOPE_VERSION, KeelCore, Request};
use keel_core_stub::KeelCoreStub;
use serde_json::Value;
use std::path::PathBuf;

/// Subset match: objects require listed keys to match recursively; arrays
/// must match exactly; scalars must be equal.
fn subset_mismatches(actual: &Value, expected: &Value, path: &str, out: &mut Vec<String>) {
    match expected {
        Value::Object(exp) => match actual {
            Value::Object(act) => {
                for (k, v) in exp {
                    match act.get(k) {
                        None => out.push(format!("{path}.{k}: missing (expected {v})")),
                        Some(a) => subset_mismatches(a, v, &format!("{path}.{k}"), out),
                    }
                }
            }
            other => out.push(format!("{path}: expected object, got {other}")),
        },
        Value::Array(exp) => match actual {
            Value::Array(act) if act.len() == exp.len() => {
                for (i, (a, e)) in act.iter().zip(exp).enumerate() {
                    subset_mismatches(a, e, &format!("{path}[{i}]"), out);
                }
            }
            other => out.push(format!("{path}: expected {expected}, got {other}")),
        },
        scalar => {
            if actual != scalar {
                out.push(format!("{path}: expected {scalar}, got {actual}"));
            }
        }
    }
}

fn run_scenario(scenario: &Value) -> Vec<String> {
    let mut core = KeelCoreStub::new();
    let policy = &scenario["policy"];
    let want_cfg_err = scenario
        .get("expect_configure_error")
        .and_then(Value::as_str);
    match (core.configure(policy), want_cfg_err) {
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
    let steps = scenario["steps"]
        .as_array()
        .expect("steps must be an array");
    for (i, step) in steps.iter().enumerate() {
        let label = format!("step[{i}]");
        if let Some(ms) = step.get("advance_ms").and_then(Value::as_u64) {
            core.advance_clock(ms);
        } else if let Some(expect) = step.get("report_expect") {
            let mut m = Vec::new();
            subset_mismatches(&core.report(), expect, "$", &mut m);
            failures.extend(m.into_iter().map(|s| format!("{label} report: {s}")));
        } else if let Some(call) = step.get("call") {
            let target = call["target"].as_str().expect("call.target").to_string();
            let req_json = call.get("request").cloned().unwrap_or_default();
            let request = Request {
                v: ENVELOPE_VERSION,
                target: target.clone(),
                op: req_json
                    .get("op")
                    .and_then(Value::as_str)
                    .unwrap_or(&target)
                    .to_string(),
                idempotent: req_json
                    .get("idempotent")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
                args_hash: req_json
                    .get("args_hash")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            };
            let script: Vec<AttemptResult> = call
                .get("effect")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|v| serde_json::from_value(v).expect("bad effect entry"))
                .collect();
            let mut consumed = 0usize;
            let outcome = core.execute(&request, &mut |attempt| {
                assert!(
                    consumed < script.len(),
                    "{label}: effect script exhausted (attempt {attempt}, scripted {})",
                    script.len()
                );
                let res = script[consumed].clone();
                consumed += 1;
                res
            });
            if consumed != script.len() {
                failures.push(format!(
                    "{label}: effect script not fully consumed ({consumed}/{} attempts used)",
                    script.len()
                ));
            }
            if let Some(expect) = call.get("expect") {
                let actual = serde_json::to_value(&outcome).unwrap();
                let mut m = Vec::new();
                subset_mismatches(&actual, expect, "$", &mut m);
                failures.extend(m.into_iter().map(|s| format!("{label} outcome: {s}")));
            }
        } else {
            failures.push(format!("{label}: unknown step"));
        }
    }
    failures
}

#[test]
fn conformance() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../conformance/scenarios");
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no scenarios found in {}", dir.display());

    let mut failed = Vec::new();
    for f in &files {
        let scenario: Value = serde_json::from_str(&std::fs::read_to_string(f).unwrap()).unwrap();
        let name = scenario["name"].as_str().unwrap_or("?").to_string();
        let mismatches = run_scenario(&scenario);
        if mismatches.is_empty() {
            println!("ok    {name}");
        } else {
            println!("FAIL  {name}");
            for m in &mismatches {
                println!("      {m}");
            }
            failed.push(name);
        }
    }
    assert!(
        failed.is_empty(),
        "{}/{} scenarios failed: {failed:?}",
        failed.len(),
        files.len()
    );
}
