//! keel-ffi: the frozen C ABI facade over [`keel_core::Engine`], implementing
//! `contracts/core-ffi.h`. This is the surface the PyO3/napi bindings wrap and
//! the CLI links directly; it is the **one** crate in the workspace permitted
//! `unsafe` (see the crate manifest for why the workspace `unsafe_code = forbid`
//! lint is restated-minus-unsafe here rather than inherited).
//!
//! # Envelopes
//!
//! Requests and outcomes cross the boundary as MessagePack maps (rmp-serde,
//! field names as keys via `to_vec_named`) of the normative `keel-core-api`
//! types. Every envelope carries `"v": 1`; unknown fields are ignored by
//! readers (plain serde behavior — see the `unknown_fields_are_ignored` test),
//! and an unsupported `"v"` on a request fails [`keel_execute`] with
//! `KEEL_E004`. Two surfaces are JSON, not MessagePack, exactly as the header
//! specifies: [`keel_configure`]'s policy input and its `{code, message}`
//! diagnostic, and [`keel_report`]'s deterministic sorted-key output.
//!
//! ## The AttemptRequest envelope
//!
//! The effect callback ([`KeelEffectFn`], the header's `keel_effect_fn`) is
//! handed one attempt's request as a MessagePack map with these keys:
//!
//! - `v` (u32) — envelope version, always `1`;
//! - `attempt` (u32) — 1-based attempt number (also passed as a bare argument);
//! - `target` (string) — the originating [`Request::target`];
//! - `op` (string) — the originating [`Request::op`].
//!
//! It writes an [`AttemptResult`] envelope (MessagePack) into `*result_out` and
//! returns `0`; a nonzero return means the callback itself could not run and the
//! core synthesizes `AttemptResult::Error { class: other }`.
//!
//! # Buffer ownership (both directions)
//!
//! - **Core → caller**: [`keel_configure`] (its diagnostic), [`keel_execute`]
//!   (the Outcome) and [`keel_report`] write core-allocated [`KeelBuf`]s the
//!   caller MUST release exactly once with [`keel_buf_free`].
//! - **Callee → core**: the effect callback's `*result_out` is allocated by the
//!   callee with its own allocator. The core copies the bytes before returning
//!   and NEVER frees that buffer — the callee owns it.
//!
//! # Threading & blocking
//!
//! A [`KeelCore`] handle is internally synchronized (a `Mutex` around its
//! runtime+engine), so every function is callable from any thread; calls on one
//! handle are serialized. [`keel_execute`] drives the engine to completion with
//! `block_on`, so it blocks its caller for the call's backoff/throttle waits —
//! the synchronous contract the async bridge wraps in a later slice.
//!
//! # Panics never cross the ABI
//!
//! Every entry point wraps its body in [`std::panic::catch_unwind`]; a panic
//! becomes `KEEL_E040` (or a null handle / no-op for the pointer-returning and
//! `void` functions).
//!
//! # Test clock (`test-clock` feature)
//!
//! The `test-clock` feature adds [`keel_test_new_paused`] and
//! [`keel_test_advance_clock`], which are **not** in `core-ffi.h` and are
//! feature-gated out of every release build. They exist so the conformance
//! harness can drive the engine on tokio's paused virtual clock.

use core::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::slice;
use std::sync::Mutex;
#[cfg(feature = "test-clock")]
use std::time::Duration;

use keel_core::Engine;
use keel_core_api::{AttemptResult, ENVELOPE_VERSION, ErrorClass, ErrorCode, KeelError, Request};
use serde::Serialize;
use tokio::runtime::{Builder, Runtime};

/// Return codes mirroring the `KeelErrorCode` enum in `contracts/core-ffi.h`.
/// Only the codes the facade actually returns are named; the rest live inside
/// Outcome/diagnostic envelopes, not in return values.
pub const KEEL_OK: i32 = 0;
/// Policy document failed validation (returned by [`keel_configure`]).
pub const KEEL_E001_POLICY_INVALID: i32 = 1;
/// A request/outcome envelope failed to decode (returned by [`keel_execute`]).
pub const KEEL_E003_ENVELOPE_DECODE: i32 = 3;
/// Unsupported envelope `"v"` (returned by [`keel_execute`]).
pub const KEEL_E004_ENVELOPE_VERSION: i32 = 4;
/// Internal failure — a caught panic or an impossible serialization error.
pub const KEEL_E040_INTERNAL: i32 = 40;

/// A byte buffer crossing the boundary (`contracts/core-ffi.h` `KeelBuf`).
/// Core-allocated instances are released with [`keel_buf_free`].
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct KeelBuf {
    /// Pointer to `len` bytes, or null for "no buffer".
    pub data: *mut u8,
    /// Length in bytes (`size_t`).
    pub len: usize,
}

impl KeelBuf {
    /// The null/empty buffer, used to initialize caller-provided out-slots.
    pub const EMPTY: KeelBuf = KeelBuf {
        data: ptr::null_mut(),
        len: 0,
    };
}

/// The effect callback type (`contracts/core-ffi.h` `keel_effect_fn`). `Option`
/// so a null pointer is representable; a null callback fails [`keel_execute`]
/// with `KEEL_E040`. See the crate docs for the AttemptRequest envelope and the
/// buffer-ownership contract.
pub type KeelEffectFn = Option<
    unsafe extern "C" fn(
        userdata: *mut c_void,
        attempt: u32,
        request: *const u8,
        request_len: usize,
        result_out: *mut KeelBuf,
    ) -> i32,
>;

/// Opaque core handle (`contracts/core-ffi.h` `KeelCore`). Internally
/// synchronized: the mutex serializes every call on the handle.
#[derive(Debug)]
pub struct KeelCore {
    inner: Mutex<Inner>,
}

/// The runtime+engine pair a handle owns. The current-thread runtime drives
/// `Engine::execute` under `block_on`.
#[derive(Debug)]
struct Inner {
    runtime: Runtime,
    engine: Engine,
}

impl KeelCore {
    /// Build a handle around an already-constructed runtime. `Engine::new`
    /// captures a `tokio::time::Instant`, so it is constructed *inside* the
    /// runtime — under `test-clock` that anchors the report's `clock_ms` epoch
    /// to the paused clock.
    fn build(runtime: Runtime) -> Self {
        let engine = runtime.block_on(async { Engine::new() });
        Self {
            inner: Mutex::new(Inner { runtime, engine }),
        }
    }
}

/// The AttemptRequest envelope (see crate docs). Borrows the originating
/// request's strings so no allocation is needed beyond the MessagePack buffer.
#[derive(Debug, Serialize)]
struct AttemptRequest<'a> {
    v: u32,
    attempt: u32,
    target: &'a str,
    op: &'a str,
}

/// Borrow a `(ptr, len)` C buffer as a slice, treating a null pointer or zero
/// length as the empty slice.
///
/// # Safety
/// When `ptr` is non-null and `len > 0`, `ptr` must be valid for reads of `len`
/// bytes for the returned slice's lifetime.
unsafe fn read_slice<'a>(ptr: *const u8, len: usize) -> &'a [u8] {
    if ptr.is_null() || len == 0 {
        &[]
    } else {
        // SAFETY: guaranteed valid for `len` bytes by the caller (above).
        unsafe { slice::from_raw_parts(ptr, len) }
    }
}

/// Move an owned byte buffer out to the caller as a [`KeelBuf`]. The allocation
/// is a `Box<[u8]>` (capacity == length), so [`keel_buf_free`] can rebuild and
/// drop the exact same allocation.
fn into_keel_buf(bytes: Vec<u8>) -> KeelBuf {
    let boxed: Box<[u8]> = bytes.into_boxed_slice();
    let len = boxed.len();
    // `Box::into_raw` carries the allocation's provenance; cast fat -> thin.
    let data = Box::into_raw(boxed).cast::<u8>();
    KeelBuf { data, len }
}

/// Synthesize the `Error { class: other }` the header prescribes when the effect
/// callback cannot produce a usable result.
fn synth_other(message: String) -> AttemptResult {
    AttemptResult::Error {
        class: ErrorClass::Other,
        http_status: None,
        retry_after_ms: None,
        message,
        original: None,
    }
}

/// Serialize a `{code, message}` diagnostic into a caller-provided out-slot,
/// which the caller frees with [`keel_buf_free`]. A null slot is ignored (the
/// header makes `err_out` optional).
///
/// # Safety
/// `err_out` must be null or point to a writable [`KeelBuf`].
unsafe fn write_diagnostic(err_out: *mut KeelBuf, err: &KeelError) {
    // SAFETY: caller guarantees `err_out` is null or a writable slot.
    let Some(slot) = (unsafe { err_out.as_mut() }) else {
        return;
    };
    let json = serde_json::to_vec(err).unwrap_or_else(|_| {
        br#"{"code":"KEEL-E040","message":"diagnostic serialization failed"}"#.to_vec()
    });
    *slot = into_keel_buf(json);
}

/// Bridge one attempt to the C effect callback: encode the AttemptRequest,
/// invoke the callee, and decode its AttemptResult. Any failure of the callback
/// contract (nonzero return, missing buffer, undecodable result) degrades to
/// `Error { class: other }` — the callback can never fail the core itself.
fn invoke_effect(
    effect: unsafe extern "C" fn(*mut c_void, u32, *const u8, usize, *mut KeelBuf) -> i32,
    userdata: *mut c_void,
    attempt: u32,
    target: &str,
    op: &str,
) -> AttemptResult {
    let request = AttemptRequest {
        v: ENVELOPE_VERSION,
        attempt,
        target,
        op,
    };
    let bytes = match rmp_serde::to_vec_named(&request) {
        Ok(bytes) => bytes,
        Err(e) => return synth_other(format!("failed to encode AttemptRequest: {e}")),
    };
    let mut result_out = KeelBuf::EMPTY;
    // SAFETY: `effect` is a valid, non-null callback (checked by keel_execute);
    // `bytes` is valid for `bytes.len()` bytes for the duration of the call; and
    // `result_out` is a live, writable slot. Per the header the callee allocates
    // the written buffer; we copy it below and never free it.
    let rc = unsafe {
        effect(
            userdata,
            attempt,
            bytes.as_ptr(),
            bytes.len(),
            &raw mut result_out,
        )
    };
    if rc != 0 {
        return synth_other(format!("effect callback failed (rc = {rc})"));
    }
    if result_out.data.is_null() {
        return synth_other("effect callback returned success but wrote no result".to_owned());
    }
    // Copy before return (header): read the callee's buffer into an owned Vec.
    // SAFETY: on a zero return the callee guarantees `result_out.data` points to
    // `result_out.len` initialized bytes that stay valid until it returns.
    let copied = unsafe { slice::from_raw_parts(result_out.data, result_out.len) }.to_vec();
    match rmp_serde::from_slice::<AttemptResult>(&copied) {
        Ok(result) => result,
        Err(e) => synth_other(format!("effect result envelope undecodable: {e}")),
    }
}

/// Build a current-thread runtime with only the time driver enabled (the engine
/// uses the timer wheel, never IO). `paused` turns on tokio's virtual clock.
fn build_runtime(paused: bool) -> std::io::Result<Runtime> {
    let mut builder = Builder::new_current_thread();
    builder.enable_time();
    #[cfg(feature = "test-clock")]
    if paused {
        builder.start_paused(true);
    }
    #[cfg(not(feature = "test-clock"))]
    let _ = paused; // paused is only reachable under test-clock
    builder.build()
}

/// Create a core. Returns null only on OOM / runtime-build failure (or a caught
/// panic), per the header.
#[unsafe(no_mangle)]
pub extern "C" fn keel_new() -> *mut KeelCore {
    catch_unwind(|| {
        let Ok(runtime) = build_runtime(false) else {
            return ptr::null_mut();
        };
        Box::into_raw(Box::new(KeelCore::build(runtime)))
    })
    .unwrap_or(ptr::null_mut())
}

/// Free a core created by [`keel_new`] (or [`keel_test_new_paused`]).
///
/// # Safety
/// `core` must be null or a pointer returned by `keel_new` /
/// `keel_test_new_paused` that has not already been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn keel_free(core: *mut KeelCore) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if core.is_null() {
            return;
        }
        // SAFETY: caller guarantees `core` came from `keel_new`/
        // `keel_test_new_paused` and is freed exactly once.
        drop(unsafe { Box::from_raw(core) });
    }));
}

/// Configure (or reconfigure) the core with a UTF-8 JSON policy document.
/// Returns `KEEL_OK` or `KEEL_E001`; on error, `*err_out` (if non-null) receives
/// a `{code, message}` JSON diagnostic the caller frees with [`keel_buf_free`].
///
/// # Safety
/// `core` must be a valid handle. `policy_json` must be null or valid for
/// `policy_len` bytes. `err_out` must be null or a writable [`KeelBuf`] slot.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn keel_configure(
    core: *mut KeelCore,
    policy_json: *const u8,
    policy_len: usize,
    err_out: *mut KeelBuf,
) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: caller guarantees `core` is a valid handle (or null).
        let Some(core) = (unsafe { core.as_ref() }) else {
            return KEEL_E040_INTERNAL;
        };
        // SAFETY: caller guarantees `policy_json`/`policy_len` describe a buffer.
        let bytes = unsafe { read_slice(policy_json, policy_len) };
        let result = match serde_json::from_slice::<serde_json::Value>(bytes) {
            Ok(value) => core
                .inner
                .lock()
                .expect("keel core mutex poisoned")
                .engine
                .configure(&value),
            Err(e) => Err(KeelError {
                code: ErrorCode::PolicyInvalid,
                message: format!("policy is not valid JSON: {e}"),
            }),
        };
        match result {
            Ok(()) => KEEL_OK,
            Err(err) => {
                // SAFETY: caller guarantees `err_out` is null or writable.
                unsafe { write_diagnostic(err_out, &err) };
                KEEL_E001_POLICY_INVALID
            }
        }
    }))
    .unwrap_or(KEEL_E040_INTERNAL)
}

/// Execute one intercepted call through the target's layer chain. Always writes
/// an Outcome envelope (MessagePack) to `*outcome_out` on `KEEL_OK`; returns
/// `KEEL_E003`/`KEEL_E004` if the request envelope is undecodable or an
/// unsupported version (no Outcome written — the caller reads the return code
/// first), or `KEEL_E040` on internal failure.
///
/// # Safety
/// `core` must be a valid handle. `request` must be null or valid for
/// `request_len` bytes. `effect` must be null or a valid callback honoring the
/// AttemptRequest/AttemptResult contract. `userdata` is passed through to
/// `effect` untouched. `outcome_out` must be a writable [`KeelBuf`] slot.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn keel_execute(
    core: *mut KeelCore,
    request: *const u8,
    request_len: usize,
    effect: KeelEffectFn,
    userdata: *mut c_void,
    outcome_out: *mut KeelBuf,
) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: caller guarantees `core` is a valid handle (or null).
        let Some(core) = (unsafe { core.as_ref() }) else {
            return KEEL_E040_INTERNAL;
        };
        // SAFETY: caller guarantees `outcome_out` is null or a writable slot.
        let Some(slot) = (unsafe { outcome_out.as_mut() }) else {
            return KEEL_E040_INTERNAL;
        };
        let Some(effect) = effect else {
            return KEEL_E040_INTERNAL; // a null callback cannot perform attempts
        };
        // SAFETY: caller guarantees `request`/`request_len` describe a buffer.
        let bytes = unsafe { read_slice(request, request_len) };
        let Ok(req) = rmp_serde::from_slice::<Request>(bytes) else {
            return KEEL_E003_ENVELOPE_DECODE;
        };
        if req.v != ENVELOPE_VERSION {
            return KEEL_E004_ENVELOPE_VERSION;
        }

        let guard = core.inner.lock().expect("keel core mutex poisoned");
        let request_ref = &req;
        // block_on is synchronous (no `.await` here), so holding the std mutex
        // guard across it is fine — it simply serializes calls on this handle.
        let outcome =
            guard
                .runtime
                .block_on(guard.engine.execute(&req, async move |attempt: u32| {
                    invoke_effect(
                        effect,
                        userdata,
                        attempt,
                        request_ref.target.as_str(),
                        request_ref.op.as_str(),
                    )
                }));
        drop(guard);

        let encoded =
            rmp_serde::to_vec_named(&outcome).expect("Outcome serialization is infallible");
        *slot = into_keel_buf(encoded);
        KEEL_OK
    }))
    .unwrap_or(KEEL_E040_INTERNAL)
}

/// Write the deterministic (sorted-key) metrics/discovery report as UTF-8 JSON
/// into `*json_out` (freed by the caller with [`keel_buf_free`]). Returns
/// `KEEL_OK` or `KEEL_E040`.
///
/// # Safety
/// `core` must be a valid handle. `json_out` must be a writable [`KeelBuf`] slot.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn keel_report(core: *mut KeelCore, json_out: *mut KeelBuf) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: caller guarantees `core` is a valid handle (or null).
        let Some(core) = (unsafe { core.as_ref() }) else {
            return KEEL_E040_INTERNAL;
        };
        // SAFETY: caller guarantees `json_out` is null or a writable slot.
        let Some(slot) = (unsafe { json_out.as_mut() }) else {
            return KEEL_E040_INTERNAL;
        };
        // `report()` reads `tokio::time::Instant::now()` for `clock_ms`; run it
        // inside the runtime so that read sees this handle's clock (the paused
        // virtual clock under `test-clock`), not an ambient one.
        let guard = core.inner.lock().expect("keel core mutex poisoned");
        let report = guard.runtime.block_on(async { guard.engine.report() });
        drop(guard);
        // `report()` is backed by BTreeMap-keyed maps, so serialization is
        // sorted-key deterministic — exactly the header's report contract.
        let Ok(json) = serde_json::to_vec(&report) else {
            return KEEL_E040_INTERNAL;
        };
        *slot = into_keel_buf(json);
        KEEL_OK
    }))
    .unwrap_or(KEEL_E040_INTERNAL)
}

/// Release a core-allocated buffer exactly once.
///
/// # Safety
/// `buf` must be a [`KeelBuf`] returned by this library (via `keel_configure`,
/// `keel_execute`, or `keel_report`) and not already freed, or the null buffer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn keel_buf_free(buf: KeelBuf) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if buf.data.is_null() {
            return;
        }
        // SAFETY: a non-null `data`/`len` was produced by `into_keel_buf` from
        // `Box::<[u8]>::into_raw`, so rebuilding that exact `Box<[u8]>` over the
        // same pointer+length and dropping it reclaims precisely that allocation.
        // Freed at most once (the header's ownership rule).
        let slice = ptr::slice_from_raw_parts_mut(buf.data, buf.len);
        drop(unsafe { Box::from_raw(slice) });
    }));
}

/// Create a core whose runtime runs on tokio's paused virtual clock. Harness
/// only — not declared in `core-ffi.h`, feature-gated out of release builds.
#[cfg(feature = "test-clock")]
#[unsafe(no_mangle)]
pub extern "C" fn keel_test_new_paused() -> *mut KeelCore {
    catch_unwind(|| {
        let Ok(runtime) = build_runtime(true) else {
            return ptr::null_mut();
        };
        Box::into_raw(Box::new(KeelCore::build(runtime)))
    })
    .unwrap_or(ptr::null_mut())
}

/// Advance the paused virtual clock by `ms` milliseconds. Harness only (see
/// [`keel_test_new_paused`]); requires a core built by `keel_test_new_paused`.
///
/// # Safety
/// `core` must be null or a valid handle created by [`keel_test_new_paused`].
#[cfg(feature = "test-clock")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn keel_test_advance_clock(core: *mut KeelCore, ms: u64) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: caller guarantees `core` is a valid handle (or null).
        let Some(core) = (unsafe { core.as_ref() }) else {
            return;
        };
        let guard = core.inner.lock().expect("keel core mutex poisoned");
        guard.runtime.block_on(async {
            tokio::time::advance(Duration::from_millis(ms)).await;
        });
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unknown envelope fields must be ignored by the decoder (serde default).
    #[test]
    fn unknown_fields_are_ignored() {
        // A Request map with an extra, undeclared "surprise" key.
        let mut buf = Vec::new();
        let mut map = std::collections::BTreeMap::new();
        map.insert("v", serde_json::json!(1));
        map.insert("target", serde_json::json!("api.example.com"));
        map.insert("op", serde_json::json!("GET /x"));
        map.insert("idempotent", serde_json::json!(true));
        map.insert("surprise", serde_json::json!("ignored"));
        rmp_serde::encode::write_named(&mut buf, &map).expect("encode");
        let req: Request = rmp_serde::from_slice(&buf).expect("decode ignores unknown fields");
        assert_eq!(req.target, "api.example.com");
        assert_eq!(req.v, ENVELOPE_VERSION);
    }

    /// `into_keel_buf` + `keel_buf_free` round-trips without leaking or
    /// double-freeing (run under miri/leak-check for the real proof).
    #[test]
    fn buf_round_trip() {
        let buf = into_keel_buf(b"hello".to_vec());
        assert_eq!(buf.len, 5);
        // SAFETY: `buf` came straight from `into_keel_buf` and is freed once.
        unsafe { keel_buf_free(buf) };
        // The null buffer is a safe no-op.
        unsafe { keel_buf_free(KeelBuf::EMPTY) };
    }
}
