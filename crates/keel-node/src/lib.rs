//! `keel-core-native`: the napi-rs addon wrapping [`keel_engine::Engine`] for the
//! Keel Node front end. It exposes the same logical surface as the pure-JS
//! `node/keel-core-stub` (the conformance referee) so the two are interchangeable
//! behind `node/keel/src/backend.mjs`:
//!
//! - `KeelCore.configure(policy)` — throws a JS `Error` whose `.code` is the
//!   stable `"KEEL-E0NN"` (via [`napi::Env::throw_error`]), mirroring the stub's
//!   `KeelError`.
//! - `KeelCore.execute(request, effect)` — **synchronous**. Runs the engine on a
//!   per-handle current-thread runtime under `block_on`; the JS `effect(attempt)`
//!   callback is invoked directly on the main thread and returns an
//!   attempt-result object.
//! - `KeelCore.executeAsync(request, effect)` — returns a `Promise`; the async
//!   `effect(attempt)` coroutine is awaited on the caller's libuv loop via a
//!   [`ThreadsafeFunction`] whose return type is a JS `Promise`.
//! - `KeelCore.report()` — the deterministic report object.
//! - `KeelCore.advanceClock(ms)` and `new KeelCore({ paused: true })` —
//!   **harness-only** virtual-clock controls (always present, not part of the
//!   production surface).
//!
//! # Runtime & clock (mirrors `keel-ffi` / `keel-py`)
//!
//! Each handle owns a current-thread tokio runtime (time driver only — the
//! engine uses the timer wheel, never IO). [`Engine::new`], `report()`, and clock
//! advancement all run **inside** that runtime under `block_on`, because they
//! read `tokio::time::Instant`; under `paused = true` that anchors and advances
//! the virtual clock the conformance suite drives. The runtime is behind a
//! `Mutex` so calls on one handle serialize (no `.await` is held across that std
//! mutex — `block_on` is synchronous from our side).
//!
//! The async path instead runs the engine future on napi's own tokio runtime
//! (real, non-paused time) so it can await the JS effect's `Promise` on the event
//! loop; the shared [`Engine`] is `&self`-concurrent, held as an `Arc`, so no
//! lock is taken across an `.await`.
//!
//! # Why the napi surface is `#[cfg(not(test))]`
//!
//! `keel-py` keeps its native surface compilable under `cargo test` because PyO3
//! links `libpython` (the `extension-module` feature is off in tests). Node has
//! no importable host library to link a *test executable* against, so the
//! `napi_*` symbols a `#[napi]` item references would be undefined at link time.
//! We therefore gate the whole napi binding module behind `#[cfg(not(test))]`: it
//! is compiled for the cdylib (the real addon) and by `cargo clippy`
//! (check-only, no link), but elided from the `cargo test` harness — whose unit
//! tests exercise only the pure, host-independent helpers below.

use std::io;

use keel_core_api::{AttemptResult, ErrorClass, Request};
use serde_json::Value;
use tokio::runtime::{Builder, Runtime};

/// The `Error { class: other }` degradation for a callback that could not produce
/// a usable [`AttemptResult`] — identical to the FFI facade's rule; the callback
/// can never crash the core.
fn synth_other(message: String) -> AttemptResult {
    AttemptResult::Error {
        class: ErrorClass::Other,
        http_status: None,
        retry_after_ms: None,
        message,
        original: None,
    }
}

/// Type a request [`Value`] as a [`Request`], defaulting a missing `idempotent`
/// to `false` (the stub's `?? false` semantics). Pure — unit-tested without a
/// Node host.
fn request_from_value(mut value: Value) -> Result<Request, String> {
    if let Some(map) = value.as_object_mut() {
        map.entry("idempotent").or_insert(Value::Bool(false));
    }
    serde_json::from_value(value).map_err(|e| e.to_string())
}

/// Decode one JS attempt-result [`Value`] into an [`AttemptResult`], degrading a
/// malformed value to `Error { class: other }` (never fails the core).
fn decode_attempt(value: Value) -> AttemptResult {
    match serde_json::from_value::<AttemptResult>(value) {
        Ok(result) => result,
        Err(e) => synth_other(format!("effect result is not a valid attempt result: {e}")),
    }
}

/// Build a current-thread runtime with only the time driver; `paused` turns on
/// tokio's virtual clock (the conformance harness's model of time).
fn build_runtime(paused: bool) -> io::Result<Runtime> {
    let mut builder = Builder::new_current_thread();
    builder.enable_time();
    if paused {
        builder.start_paused(true);
    }
    builder.build()
}

#[cfg(not(test))]
mod bindings {
    //! The napi surface. Elided from `cargo test` (see the crate docs) but linted
    //! by `cargo clippy` and compiled into the cdylib addon.

    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use keel_core_api::{AttemptResult, KeelError, Request};
    use keel_engine::Engine;
    use keel_journal::{SqliteJournal, SystemClock};
    use napi::bindgen_prelude::*;
    use napi::threadsafe_function::ThreadsafeFunction;
    use napi_derive::napi;
    use serde_json::Value;
    use tokio::runtime::Runtime;

    use super::{build_runtime, decode_attempt, request_from_value, synth_other};

    /// Open (creating the parent dir + file as needed) a WAL SQLite journal at
    /// `path` on the wall clock and attach it — enabling the `scope = persistent`
    /// dev cache so identical prompts replay across separate `keel` runs (Task 14
    /// item 1). `SystemClock` is correct here: production runs on real time, so
    /// cache-TTL expiry is measured in wall-clock ms.
    fn attach_journal(engine: &mut Engine, path: &str) -> std::result::Result<(), String> {
        if let Some(parent) = std::path::Path::new(path).parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("journal dir {}: {e}", parent.display()))?;
        }
        let journal = SqliteJournal::open(path, SystemClock)
            .map_err(|e| format!("journal open {path}: {e}"))?;
        engine.attach_journal(journal);
        Ok(())
    }

    /// The async-effect callback shape: JS `(attempt: number) => Promise<object>`.
    /// `CalleeHandled = false` (5th generic) means napi passes only `attempt` —
    /// no leading `err` argument — matching the documented effect signature; the
    /// return type is the JS `Promise` we then await.
    type AsyncEffect = ThreadsafeFunction<u32, Promise<Value>, u32, Status, false>;

    /// Set a pending JS `Error` carrying the stable `.code` (`"KEEL-E0NN"`) and
    /// return the sentinel napi error so napi does not overwrite it. This is the
    /// canonical way to throw a custom-`code` error from a `#[napi]` method.
    fn throw_keel(env: Env, code: &str, message: &str) -> Error {
        let _ = env.throw_error(message, Some(code));
        Error::new(Status::PendingException, message.to_string())
    }

    /// Throw the JS mirror of a core [`KeelError`], preserving its code.
    fn throw_keel_from(env: Env, err: &KeelError) -> Error {
        throw_keel(env, err.code.as_str(), &err.message)
    }

    /// Decode a JS request object into a [`Request`] (KEEL-E003 on failure).
    fn decode_request(env: Env, request: Value) -> Result<Request> {
        request_from_value(request)
            .map_err(|e| throw_keel(env, "KEEL-E003", &format!("request envelope invalid: {e}")))
    }

    /// Await the asynchronous JS effect for one attempt: call it through the
    /// threadsafe function (yielding its `Promise`), await that on napi's runtime,
    /// then decode. Any failure degrades to `Error { class: other }`.
    async fn invoke_async_effect(effect: &AsyncEffect, attempt: u32) -> AttemptResult {
        match effect.call_async(attempt).await {
            Ok(promise) => match promise.await {
                Ok(value) => decode_attempt(value),
                Err(err) => synth_other(format!("awaiting async effect result failed: {err}")),
            },
            Err(err) => synth_other(format!("async effect callback raised: {err}")),
        }
    }

    /// Options accepted by the `KeelCore` constructor.
    #[napi(object)]
    #[derive(Debug)]
    pub struct KeelCoreOptions {
        /// Harness-only: run on tokio's virtual clock so `advanceClock` and
        /// auto-advanced timer waits are deterministic.
        pub paused: Option<bool>,
        /// Production: attach a SQLite journal at this path (enables the
        /// `scope = persistent` dev cache — cross-run LLM replay). Absent/empty
        /// ⇒ an in-memory core. Not combined with `paused` by any caller.
        pub journal_path: Option<String>,
    }

    /// The native core handle. `engine` is the shared, `&self`-concurrent kernel;
    /// `runtime` (behind a `Mutex`) drives the synchronous / paused-clock paths.
    #[napi(js_name = "KeelCore")]
    #[derive(Debug)]
    pub struct KeelCore {
        engine: Arc<Engine>,
        runtime: Mutex<Runtime>,
        /// True when a journal is attached (the persistent dev-cache scope is live).
        persistent: bool,
    }

    #[napi]
    impl KeelCore {
        /// Create a core. `{ paused: true }` (harness-only) runs it on tokio's
        /// virtual clock; the default is real time.
        #[napi(constructor)]
        pub fn new(options: Option<KeelCoreOptions>) -> Result<Self> {
            let paused = options.as_ref().and_then(|o| o.paused).unwrap_or(false);
            let journal_path = options.and_then(|o| o.journal_path);
            let runtime = build_runtime(paused)
                .map_err(|e| Error::from_reason(format!("KEEL-E040: runtime build failed: {e}")))?;
            // `Engine::new` reads `tokio::time::Instant::now()`; build it inside
            // the runtime so a paused clock's epoch is anchored (see `keel-ffi`).
            let mut engine = runtime.block_on(async { Engine::new() });
            let persistent = match journal_path {
                Some(path) if !path.is_empty() => {
                    attach_journal(&mut engine, &path)
                        .map_err(|e| Error::from_reason(format!("KEEL-E040: {e}")))?;
                    true
                }
                _ => false,
            };
            Ok(Self {
                engine: Arc::new(engine),
                runtime: Mutex::new(runtime),
                persistent,
            })
        }

        /// Whether a persistent journal is attached (the `scope = persistent`
        /// dev cache is live). The front end reads this to decide whether to emit
        /// `scope = "persistent"` for the LLM dev cache (cross-run replay).
        #[napi(getter)]
        pub fn persistent(&self) -> bool {
            self.persistent
        }

        /// Apply a policy document (object, per `policy.schema.json`). Throws a JS
        /// `Error` with `.code = "KEEL-E001"` and the offending field path.
        #[allow(
            clippy::needless_pass_by_value,
            reason = "napi decodes the JS object into an owned serde_json::Value arg; the engine borrows it"
        )]
        #[napi]
        pub fn configure(&self, env: Env, policy: Value) -> Result<()> {
            self.engine
                .configure(&policy)
                .map_err(|e| throw_keel_from(env, &e))
        }

        /// Run one intercepted call synchronously. The engine drives the layer
        /// chain on this handle's runtime; `effect(attempt)` is invoked on the
        /// main thread and returns an attempt-result object. Always returns an
        /// outcome object (engine-level failures are reported *in* the outcome).
        #[allow(
            clippy::needless_pass_by_value,
            reason = "napi passes the callback as an owned Function handle; it is called by reference per attempt"
        )]
        #[napi(ts_args_type = "request: object, effect: (attempt: number) => object")]
        pub fn execute(
            &self,
            env: Env,
            request: Value,
            effect: Function<'_, u32, Value>,
        ) -> Result<Value> {
            let request = decode_request(env, request)?;
            let guard = self
                .runtime
                .lock()
                .expect("keel-node runtime mutex poisoned");
            // Holding the runtime mutex across the synchronous `block_on`
            // serializes calls on this handle; no `.await` is held across it.
            let outcome = guard.block_on(self.engine.execute(&request, async |attempt: u32| {
                match effect.call(attempt) {
                    Ok(value) => decode_attempt(value),
                    Err(err) => synth_other(format!("effect callback raised: {err}")),
                }
            }));
            drop(guard);
            serde_json::to_value(&outcome)
                .map_err(|e| throw_keel(env, "KEEL-E040", &format!("outcome not encodable: {e}")))
        }

        /// Run one intercepted call asynchronously, returning a `Promise` that
        /// resolves to the outcome object. The request is decoded in a SYNC
        /// prelude that still holds `Env`, so a malformed envelope rejects with a
        /// proper `.code = "KEEL-E003"` (parity with the sync `execute` — Task 14
        /// item 2); only then is the engine future spawned on napi's runtime via
        /// `Env::spawn_future`. `effect(attempt)` is an `async` JS function
        /// awaited on the caller's libuv loop, so it may perform real async IO.
        /// Runs on napi's runtime (real time), not the paused handle clock.
        #[napi(ts_args_type = "request: object, effect: (attempt: number) => Promise<object>")]
        pub fn execute_async<'env>(
            &self,
            env: &'env Env,
            request: Value,
            effect: AsyncEffect,
        ) -> Result<PromiseRaw<'env, Value>> {
            // Sync prelude (holds `Env`): a bad envelope throws a proper `.code`.
            let request = decode_request(*env, request)?;
            let engine = Arc::clone(&self.engine);
            // `ThreadsafeFunction` is not `Clone`; wrap it once in an `Arc` so each
            // attempt gets an owned handle. A plain `FnMut` returning an
            // owned-capture `async move` future keeps every per-attempt future
            // `'static` + `Send`, as napi's runtime requires (mirrors keel-py).
            env.spawn_future(async move {
                let effect = Arc::new(effect);
                let outcome = engine
                    .execute(&request, move |attempt: u32| {
                        let effect = Arc::clone(&effect);
                        async move { invoke_async_effect(&effect, attempt).await }
                    })
                    .await;
                serde_json::to_value(&outcome).map_err(|e| {
                    Error::from_reason(format!("KEEL-E040: outcome not encodable: {e}"))
                })
            })
        }

        /// The deterministic per-target metrics/discovery report. Read inside the
        /// handle runtime so `clock_ms` reflects its (possibly paused) clock.
        #[napi]
        pub fn report(&self) -> Value {
            let guard = self
                .runtime
                .lock()
                .expect("keel-node runtime mutex poisoned");
            guard.block_on(async { self.engine.report() })
        }

        /// Harness-only: advance the paused virtual clock by `ms` milliseconds.
        /// Requires a `{ paused: true }` handle (tokio panics otherwise). `u32`
        /// (a plain JS `number`) covers every virtual-clock advance the suite
        /// needs — ~49 days of milliseconds.
        #[napi]
        pub fn advance_clock(&self, ms: u32) {
            let guard = self
                .runtime
                .lock()
                .expect("keel-node runtime mutex poisoned");
            guard.block_on(async move {
                tokio::time::advance(Duration::from_millis(u64::from(ms))).await;
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{build_runtime, decode_attempt, request_from_value};
    use keel_core_api::AttemptResult;
    use serde_json::json;

    #[test]
    fn build_runtime_paused_and_live() {
        // Both flavors build and drive a trivial future; `paused` anchors tokio's
        // virtual clock (the harness model), `!paused` uses real time.
        for paused in [false, true] {
            let rt = build_runtime(paused).expect("runtime builds");
            let two = rt.block_on(async { 1 + 1 });
            assert_eq!(two, 2);
        }
    }

    #[test]
    fn request_defaults_idempotent_false() {
        let req = request_from_value(json!({
            "v": 1, "target": "api.example.com", "op": "GET x", "args_hash": "h1"
        }))
        .expect("valid request");
        assert!(!req.idempotent);
        assert_eq!(req.target, "api.example.com");
        assert_eq!(req.args_hash.as_deref(), Some("h1"));
    }

    #[test]
    fn request_preserves_idempotent_true() {
        let req = request_from_value(json!({
            "v": 1, "target": "t", "op": "op", "idempotent": true
        }))
        .expect("valid request");
        assert!(req.idempotent);
        assert!(req.args_hash.is_none());
    }

    #[test]
    fn request_missing_target_is_error() {
        let err = request_from_value(json!({ "v": 1, "op": "op" }))
            .expect_err("missing target must fail");
        assert!(err.contains("target"), "unexpected error: {err}");
    }

    #[test]
    fn attempt_ok_decodes() {
        let res = decode_attempt(json!({ "status": "ok", "payload": { "items": 3 } }));
        assert!(matches!(res, AttemptResult::Ok { .. }));
    }

    #[test]
    fn attempt_malformed_degrades_to_other() {
        // Missing `status`: not a valid AttemptResult -> Error { class: other }.
        let res = decode_attempt(json!({ "nonsense": true }));
        match res {
            AttemptResult::Error { class, .. } => {
                assert_eq!(class, keel_core_api::ErrorClass::Other);
            }
            AttemptResult::Ok { .. } => panic!("expected degraded error"),
        }
    }
}
