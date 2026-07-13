//! Compile-time proof of `#[keel::wrap]`'s guardrails (module docs in
//! `crates/keel-macros/src/lib.rs`): a valid usage compiles, and each
//! documented v1 restriction produces a targeted `compile_error!`, not a
//! confusing downstream type error.
//!
//! `trybuild` compiles each `tests/ui/*.rs` file as its own crate with
//! `keel` as a dependency (this file's own package), so `#[keel::wrap]` is
//! exercised exactly as a real user would write it.

#[test]
fn ui() {
    let t = trybuild::TestCases::new();
    t.pass("tests/ui/pass_basic.rs");
    t.compile_fail("tests/ui/fail_missing_target.rs");
    t.compile_fail("tests/ui/fail_sync_fn.rs");
    t.compile_fail("tests/ui/fail_non_result_return.rs");
}
