//! Frozen-ABI drift guard: the `keel_*` C functions exported by this crate must
//! be exactly those declared in `contracts/core-ffi.h`. The header is parsed
//! read-only (never modified); the signatures are additionally checked at
//! compile time by binding each export to its header-shaped function-pointer
//! type below.

use std::collections::BTreeSet;

/// The frozen header, included at compile time (read-only). The path mirrors
/// `keel-core-api`'s `include!("../../../contracts/core_api.rs")`.
const HEADER: &str = include_str!("../../../contracts/core-ffi.h");

/// Extract the `keel_*` C *function* names declared in the header: a lower-case
/// `keel_` identifier immediately followed (modulo whitespace) by `(`. This
/// matches the six function declarations and excludes the `keel_effect_fn`
/// typedef/parameter (never followed by `(`), the upper-case `KEEL_*` macros and
/// enum members, and prose mentions in comments.
fn declared_fns(header: &str) -> BTreeSet<String> {
    let bytes = header.as_bytes();
    let mut names = BTreeSet::new();
    let mut cursor = 0;
    while let Some(rel) = header[cursor..].find("keel_") {
        let start = cursor + rel;
        let mut end = start;
        while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
            end += 1;
        }
        let mut after = end;
        while after < bytes.len() && bytes[after].is_ascii_whitespace() {
            after += 1;
        }
        if after < bytes.len() && bytes[after] == b'(' {
            names.insert(header[start..end].to_owned());
        }
        cursor = end;
    }
    names
}

#[test]
fn exports_match_header() {
    let declared = declared_fns(HEADER);
    let expected: BTreeSet<String> = [
        "keel_buf_free",
        "keel_configure",
        "keel_execute",
        "keel_free",
        "keel_new",
        "keel_report",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    assert_eq!(
        declared, expected,
        "exported C symbols drifted from contracts/core-ffi.h"
    );

    // The test-clock symbols must NOT appear in the public header.
    assert!(
        !declared.contains("keel_test_new_paused") && !declared.contains("keel_test_advance_clock"),
        "test-clock symbols must stay out of the frozen header"
    );
}

/// Bind every export to its header-shaped function-pointer type. This fails to
/// compile if a signature drifts from the header — a stronger check than the
/// name-set comparison, and it forces each symbol to actually exist and be
/// `pub`. (No numeric fn casts: those trip pedantic clippy.)
#[test]
fn export_signatures_match_header() {
    use core::ffi::c_void;
    use keel_ffi::{KeelBuf, KeelCore, KeelEffectFn};

    let new_fn: extern "C" fn() -> *mut KeelCore = keel_ffi::keel_new;
    let free_fn: unsafe extern "C" fn(*mut KeelCore) = keel_ffi::keel_free;
    let configure_fn: unsafe extern "C" fn(*mut KeelCore, *const u8, usize, *mut KeelBuf) -> i32 =
        keel_ffi::keel_configure;
    let execute_fn: unsafe extern "C" fn(
        *mut KeelCore,
        *const u8,
        usize,
        KeelEffectFn,
        *mut c_void,
        *mut KeelBuf,
    ) -> i32 = keel_ffi::keel_execute;
    let report_fn: unsafe extern "C" fn(*mut KeelCore, *mut KeelBuf) -> i32 = keel_ffi::keel_report;
    let buf_free_fn: unsafe extern "C" fn(KeelBuf) = keel_ffi::keel_buf_free;

    // Reference each binding so the coercions type-check and nothing is "unused":
    // the value is the compile-time signature match, not any runtime effect.
    let _ = (
        new_fn,
        free_fn,
        configure_fn,
        execute_fn,
        report_fn,
        buf_free_fn,
    );
}
