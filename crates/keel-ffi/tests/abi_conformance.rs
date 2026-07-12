//! The 15 conformance scenarios (`conformance/scenarios/*.json`) driven end to
//! end through the frozen C ABI — the exported `extern "C"` symbols, hand-built
//! MessagePack envelopes, and a real `keel_effect_fn` C callback. This is the
//! point of the FFI crate: the SAME corpus the stub and the in-process engine
//! pass must also pass across the ABI boundary.
//!
//! Requires the `test-clock` feature: `keel_test_new_paused` runs the engine on
//! tokio's paused virtual clock and `keel_test_advance_clock` maps the scenario
//! `advance_ms` steps onto it. The engine's own backoff/throttle sleeps
//! auto-advance that clock while `keel_execute` is blocked, so `clock_ms` in the
//! report matches the in-process harness bit for bit.

#![cfg(feature = "test-clock")]

use core::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::slice;

use keel_conformance::{CallStep, Scenario, Step, load_dir, scenarios_dir, subset_mismatches};
use keel_core_api::{ENVELOPE_VERSION, Outcome};
use keel_ffi::{
    KEEL_OK, KeelBuf, KeelCore, keel_buf_free, keel_configure, keel_execute, keel_free,
    keel_report, keel_test_advance_clock, keel_test_new_paused,
};
use serde::Deserialize;
use serde_json::Value;

/// Per-call state passed as `userdata` to [`scripted_effect`].
struct EffectState {
    /// The originating request's target, checked against each AttemptRequest.
    target: String,
    /// Pre-encoded AttemptResult envelopes (MessagePack), one per scripted attempt.
    script: Vec<Vec<u8>>,
    /// How many scripted attempts the core has consumed.
    consumed: usize,
    /// Buffers we allocated and handed to the core. The core copies but never
    /// frees them (they are callee-owned); we free them here at teardown.
    handed_out: Vec<KeelBuf>,
    /// Problems observed inside the callback (bad envelope, over-consumption).
    errors: Vec<String>,
}

impl Drop for EffectState {
    fn drop(&mut self) {
        for buf in self.handed_out.drain(..) {
            if buf.data.is_null() {
                continue;
            }
            // SAFETY: each buffer came from `alloc_buf` (`Box::<[u8]>::into_raw`);
            // the core copied and never freed it, so we reclaim that same
            // `Box<[u8]>` exactly once here.
            let slice = std::ptr::slice_from_raw_parts_mut(buf.data, buf.len);
            drop(unsafe { Box::from_raw(slice) });
        }
    }
}

/// The AttemptRequest envelope the core hands the callback, decoded to prove it
/// is real and well-formed: `{v, attempt, target, op}`.
#[derive(Deserialize)]
struct AttemptRequest {
    v: u32,
    attempt: u32,
    target: String,
    op: String,
}

/// Allocate a callee-owned buffer holding `bytes`. Uses the same `Box<[u8]>`
/// scheme keel-ffi uses, so the allocation is well-formed for the round trip.
fn alloc_buf(bytes: Vec<u8>) -> KeelBuf {
    let boxed = bytes.into_boxed_slice();
    let len = boxed.len();
    let data = Box::into_raw(boxed).cast::<u8>();
    KeelBuf { data, len }
}

/// A real `keel_effect_fn`: validate the AttemptRequest envelope, then return the
/// next scripted AttemptResult in a freshly allocated, callee-owned buffer. A
/// nonzero return (script exhausted) tells the core to synthesize `Error{other}`.
unsafe extern "C" fn scripted_effect(
    userdata: *mut c_void,
    attempt: u32,
    request: *const u8,
    request_len: usize,
    result_out: *mut KeelBuf,
) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: `userdata` is the `*mut EffectState` passed to keel_execute,
        // valid and uniquely borrowed for this synchronous call.
        let state = unsafe { &mut *userdata.cast::<EffectState>() };

        // SAFETY: the core passes a buffer valid for `request_len` bytes.
        let req_bytes = unsafe { slice::from_raw_parts(request, request_len) };
        match rmp_serde::from_slice::<AttemptRequest>(req_bytes) {
            Ok(req) => {
                if req.v != ENVELOPE_VERSION {
                    state.errors.push(format!("AttemptRequest v = {}", req.v));
                }
                if req.attempt != attempt {
                    state
                        .errors
                        .push(format!("attempt arg {attempt} != envelope {}", req.attempt));
                }
                if req.target != state.target {
                    state
                        .errors
                        .push(format!("target {} != request {}", req.target, state.target));
                }
                if req.op.is_empty() {
                    state.errors.push("empty op in AttemptRequest".to_owned());
                }
            }
            Err(e) => state
                .errors
                .push(format!("AttemptRequest undecodable: {e}")),
        }

        if state.consumed >= state.script.len() {
            state
                .errors
                .push(format!("effect script exhausted at attempt {attempt}"));
            return 1; // nonzero -> core synthesizes Error{other}
        }
        let bytes = state.script[state.consumed].clone();
        state.consumed += 1;
        let buf = alloc_buf(bytes);
        // SAFETY: `result_out` is a live, writable slot for this call.
        unsafe { *result_out = buf };
        state.handed_out.push(buf);
        0
    }))
    .unwrap_or(1)
}

/// Copy a `KeelBuf`'s bytes out, treating null/empty as no bytes.
fn buf_to_vec(buf: &KeelBuf) -> Vec<u8> {
    if buf.data.is_null() || buf.len == 0 {
        Vec::new()
    } else {
        // SAFETY: a populated `KeelBuf` from this library is valid for `len`.
        unsafe { slice::from_raw_parts(buf.data, buf.len) }.to_vec()
    }
}

/// The `code` field of a `{code, message}` JSON diagnostic, if present.
fn diag_code(buf: &KeelBuf) -> Option<String> {
    let bytes = buf_to_vec(buf);
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    value.get("code")?.as_str().map(str::to_owned)
}

/// Drive one call step through `keel_execute` and record any mismatches.
fn run_call(core: *mut KeelCore, call: &CallStep, label: &str, failures: &mut Vec<String>) {
    let request = call.request();
    let req_bytes = rmp_serde::to_vec_named(&request).expect("Request to MessagePack");
    let script: Vec<Vec<u8>> = call
        .effect
        .iter()
        .map(|r| rmp_serde::to_vec_named(r).expect("AttemptResult to MessagePack"))
        .collect();
    let script_len = script.len();
    let mut state = EffectState {
        target: request.target.clone(),
        script,
        consumed: 0,
        handed_out: Vec::new(),
        errors: Vec::new(),
    };

    let mut outcome_buf = KeelBuf::EMPTY;
    // SAFETY: `core` is a live handle; `req_bytes` is valid for its length;
    // `userdata` is `&mut state`, live for the call; `outcome_buf` is writable.
    let rc = unsafe {
        keel_execute(
            core,
            req_bytes.as_ptr(),
            req_bytes.len(),
            Some(scripted_effect),
            (&raw mut state).cast::<c_void>(),
            &raw mut outcome_buf,
        )
    };
    if rc != KEEL_OK {
        failures.push(format!("{label}: keel_execute returned {rc}"));
        return;
    }

    let bytes = buf_to_vec(&outcome_buf);
    // SAFETY: `outcome_buf` was written by keel_execute; freed exactly once.
    unsafe { keel_buf_free(outcome_buf) };

    if state.consumed != script_len {
        failures.push(format!(
            "{label}: effect script not fully consumed ({}/{script_len} used)",
            state.consumed
        ));
    }
    for e in &state.errors {
        failures.push(format!("{label} effect: {e}"));
    }

    match rmp_serde::from_slice::<Outcome>(&bytes) {
        Ok(outcome) => {
            let actual = serde_json::to_value(&outcome).expect("Outcome to Value");
            let mut mismatches = Vec::new();
            subset_mismatches(&actual, &call.expect, "$", &mut mismatches);
            failures.extend(
                mismatches
                    .into_iter()
                    .map(|m| format!("{label} outcome: {m}")),
            );
        }
        Err(e) => failures.push(format!("{label}: outcome envelope undecodable: {e}")),
    }
    // `state` drops here, freeing its callee-owned effect buffers.
}

/// Drive one report step through `keel_report` and record any mismatches.
fn run_report(core: *mut KeelCore, expect: &Value, label: &str, failures: &mut Vec<String>) {
    let mut out = KeelBuf::EMPTY;
    // SAFETY: `core` is a live handle; `out` is a writable slot.
    let rc = unsafe { keel_report(core, &raw mut out) };
    if rc != KEEL_OK {
        failures.push(format!("{label}: keel_report returned {rc}"));
        return;
    }
    let bytes = buf_to_vec(&out);
    // SAFETY: `out` was written by keel_report; freed exactly once.
    unsafe { keel_buf_free(out) };
    let actual: Value = serde_json::from_slice(&bytes).expect("report is valid JSON");
    let mut mismatches = Vec::new();
    subset_mismatches(&actual, expect, "$", &mut mismatches);
    failures.extend(
        mismatches
            .into_iter()
            .map(|m| format!("{label} report: {m}")),
    );
}

/// Configure + run every step of a scenario against `core`, returning mismatches.
fn drive(scenario: &Scenario, core: *mut KeelCore) -> Vec<String> {
    let policy = serde_json::to_vec(&scenario.policy).expect("policy to JSON");
    let mut err = KeelBuf::EMPTY;
    // SAFETY: `core` is a live handle; `policy` is valid for its length; `err`
    // is a writable slot.
    let rc = unsafe { keel_configure(core, policy.as_ptr(), policy.len(), &raw mut err) };
    match scenario.expect_configure_error.as_deref() {
        Some(expected) => {
            if rc == KEEL_OK {
                return vec![format!("configure: expected {expected}, but it succeeded")];
            }
            let diag = diag_code(&err);
            // SAFETY: `err` was written by keel_configure; freed exactly once.
            unsafe { keel_buf_free(err) };
            return if diag.as_deref() == Some(expected) {
                Vec::new()
            } else {
                vec![format!("configure: expected {expected}, got {diag:?}")]
            };
        }
        None => {
            if rc != KEEL_OK {
                let diag = diag_code(&err);
                // SAFETY: `err` was written by keel_configure; freed exactly once.
                unsafe { keel_buf_free(err) };
                return vec![format!(
                    "configure: unexpected error rc={rc}, code={diag:?}"
                )];
            }
            // rc == KEEL_OK: `err` was left untouched (still EMPTY); nothing to free.
        }
    }

    let mut failures = Vec::new();
    for (i, step) in scenario.steps.iter().enumerate() {
        let label = format!("step[{i}]");
        match step {
            // SAFETY: `core` is a paused handle from keel_test_new_paused.
            Step::Advance { advance_ms } => unsafe { keel_test_advance_clock(core, *advance_ms) },
            Step::ReportExpect { report_expect } => {
                run_report(core, report_expect, &label, &mut failures);
            }
            Step::Call { call } => run_call(core, call, &label, &mut failures),
        }
    }
    failures
}

/// Run one scenario on a fresh paused core, always freeing the handle.
fn run_scenario(scenario: &Scenario) -> Vec<String> {
    // keel_test_new_paused is a safe fn (no pointer args); it returns an owned
    // handle or null.
    let core = keel_test_new_paused();
    assert!(!core.is_null(), "keel_test_new_paused returned null");
    let failures = drive(scenario, core);
    // SAFETY: `core` is the handle we just created; freed exactly once.
    unsafe { keel_free(core) };
    failures
}

#[test]
fn conformance_through_abi() {
    let scenarios = load_dir(&scenarios_dir(env!("CARGO_MANIFEST_DIR")));
    let mut failed = Vec::new();
    for (_path, scenario) in &scenarios {
        let mismatches = run_scenario(scenario);
        if mismatches.is_empty() {
            println!("ok    {}", scenario.name);
        } else {
            println!("FAIL  {}", scenario.name);
            for m in &mismatches {
                println!("      {m}");
            }
            failed.push(scenario.name.clone());
        }
    }
    assert!(
        failed.is_empty(),
        "{}/{} scenarios failed through the ABI: {failed:?}",
        failed.len(),
        scenarios.len()
    );
}
