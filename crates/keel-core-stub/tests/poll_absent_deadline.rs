//! CCR-11 coverage gap (#128 follow-up): `until.absent = "pending"` still
//! terminates at the poll deadline like any other pending verdict — the
//! deadline check in `run_poll` is on `PollVerdict::Pending` regardless of
//! whether that verdict came from the field being present-but-not-terminal
//! or from `absent` mapping a MISSING field to pending. This matters because
//! CCR-11 deliberately allows `absent = "pending"` on a host-level table,
//! where a wrong path would otherwise poll forever; the deadline is the only
//! thing bounding it. Both stubs already pin this (Python's
//! `test_absent_pending_still_reaches_the_deadline`, Node's "until.absent =
//! pending still honors the deadline") — the Rust stub did not.

use keel_core_api::{AttemptResult, ENVELOPE_VERSION, KeelCore, Request};
use keel_core_stub::KeelCoreStub;
use serde_json::json;

fn request(target: &str, idempotent: bool) -> Request {
    Request {
        v: ENVELOPE_VERSION,
        target: target.to_owned(),
        op: format!("GET {target}/x"),
        idempotent,
        args_hash: None,
    }
}

#[test]
fn poll_absent_pending_still_hits_the_deadline() {
    let mut core = KeelCoreStub::new();
    core.configure(&json!({
        "target": { "lro.example": {
            "poll": {
                "interval": "10s",
                "deadline": "25s",
                "until": { "field": "done", "terminal": [true], "absent": "pending" }
            }
        } }
    }))
    .expect("valid policy");

    // Every attempt is a running google.longrunning.Operation body: `done`
    // is never present, only `name`. Under `absent = "pending"` this must
    // keep polling — never fail open — until the deadline is exceeded.
    let outcome = core.execute(&request("lro.example", true), &mut |_attempt| {
        AttemptResult::Ok {
            payload: json!({ "name": "projects/p/operations/op1" }),
        }
    });

    assert_eq!(outcome.result, "error");
    let error = outcome.error.expect("terminal error");
    assert_eq!(error.code.as_str(), "KEEL-E016");
    assert_eq!(
        error.message,
        "GET lro.example/x poll deadline exceeded: 'done' not terminal after 25000ms"
    );
}
