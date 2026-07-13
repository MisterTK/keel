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
//! # Tier 2: durable flows (async-only, unlike `keel-py`)
//!
//! - `KeelCore.enterFlow(entrypoint, argsHash, codeHash?, explicitKey?,
//!   leaseMs?)` — open (or resume) a durable flow and make it the handle's
//!   active flow; returns `{ flowId, status, replay }`. Throws `KEEL-E030`
//!   (lease held), `KEEL-E032` (dead flow), `KEEL-E040` (no journal attached).
//! - `KeelCore.exitFlow(status)` — close the active flow (`"completed"` |
//!   `"failed"`); a no-op if none is open.
//! - `KeelCore.journalTime(key, nowMs)` / `KeelCore.journalRandom(key, bytes)`
//!   — journal (or, on replay, substitute) a virtualized clock/random read.
//!
//! Node's intercepted effects are **async-only** (`backend.mjs` drives only
//! `executeAsync`), unlike `keel-py`'s v0.1 flows, which are synchronous and
//! therefore *refuse* an async effect while a flow is open. Here `executeAsync`
//! instead routes through the open [`keel_engine::FlowHandle`]'s
//! `execute_step` when a flow is active — the async `execute_step` bridge the
//! core's module docs describe as future work. Concurrent effects inside one
//! flow are serialized in await order (`conformance/README.md`'s "Async steps
//! inside a flow" rule): `active_flow` is a [`tokio::sync::Mutex`] whose guard
//! is deliberately held for the full `execute_step` `.await` (claim through
//! terminal outcome) — the one lock in this crate that crosses an `.await`,
//! by design, because the ordering rule requires exactly one admitted step at
//! a time and an async mutex parks the *task*, not the OS thread, while a
//! second concurrent effect waits. The synchronous `execute`/`enterFlow`/
//! `exitFlow`/`journalTime`/`journalRandom` methods run on the JS thread
//! (never inside a spawned future), so `blocking_lock`/`try_lock` there is
//! safe — see each method's docs for why sync value reads use `try_lock`
//! rather than `blocking_lock` (a `Date.now`/`Math.random` read must return
//! synchronously and cannot await the admission queue without risking a
//! same-thread deadlock against an in-flight effect's `Promise` resolution).
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

    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use keel_core_api::{AttemptResult, KeelError, Request};
    use keel_engine::{Engine, FlowConfig, FlowDescriptor, FlowHandle, FlowManager};
    use keel_journal::{FlowStatus, ProcessId, SqliteJournal, SystemClock};
    use napi::bindgen_prelude::*;
    use napi::threadsafe_function::ThreadsafeFunction;
    use napi_derive::napi;
    use serde_json::{Value, json};
    use tokio::runtime::Runtime;
    use tokio::sync::Mutex as AsyncMutex;

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

    /// Best-effort OTLP span + metrics export, gated by the `otel` build feature
    /// AND [`keel_engine::otel::export_enabled`] (env `KEEL_OTEL` first, else the
    /// effective policy's `telemetry.otlp_endpoint`). OFF by default: an addon
    /// built without `--features otel` links no OpenTelemetry dependency and
    /// this is a no-op. When enabled, the FIRST call that decides to export
    /// installs the global OTLP exporter — [`keel_engine::otel::resolve_endpoint`]
    /// picks the endpoint (env wins over policy) — so the engine's
    /// spans/metrics reach a collector. Called twice per core: once at
    /// construction with no policy known yet (`policy_endpoint = None`, so only
    /// `KEEL_OTEL` + `OTEL_*` env can trigger it), and again from `configure`
    /// once the effective policy's `[telemetry]` is known; the `OnceLock` makes
    /// the second call a no-op if the first already exported. Init failures
    /// never break the process — they warn and export stays off. Best-effort:
    /// buffered spans/metrics flush as the core's runtime runs (architecture
    /// spec §4.5, otel.rs).
    #[cfg(feature = "otel")]
    static OTEL_GUARD: std::sync::OnceLock<Option<keel_engine::otel::OtelGuard>> =
        std::sync::OnceLock::new();

    #[cfg(feature = "otel")]
    fn maybe_init_otel(runtime: &Runtime, policy_endpoint: Option<&str>) {
        if !keel_engine::otel::export_enabled(policy_endpoint) {
            return;
        }
        OTEL_GUARD.get_or_init(|| {
            let endpoint = keel_engine::otel::resolve_endpoint(policy_endpoint);
            match runtime.block_on(async { keel_engine::otel::init_otlp(endpoint.as_deref()) }) {
                Ok(guard) => Some(guard),
                Err(e) => {
                    eprintln!(
                        "keel: OTel export enabled but OTLP init failed ({e}); export disabled"
                    );
                    None
                }
            }
        });
    }

    /// No-op when the `otel` feature is off (the default): no OpenTelemetry
    /// dependency is linked and the addon never touches telemetry.
    #[cfg(not(feature = "otel"))]
    fn maybe_init_otel(_runtime: &Runtime, _policy_endpoint: Option<&str>) {}

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

    /// Lock a per-handle mutex, recovering the guard even if a previous holder
    /// panicked while holding it (the guarded runtime stays valid). One panic
    /// must not permanently brick the handle with a poisoned-mutex lock-out on
    /// every later call.
    fn lock_recover<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        mutex
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Decode a JS request object into a [`Request`] (KEEL-E003 on failure).
    fn decode_request(env: Env, request: Value) -> Result<Request> {
        request_from_value(request)
            .map_err(|e| throw_keel(env, "KEEL-E003", &format!("request envelope invalid: {e}")))
    }

    /// The status token the front end reads back from `enterFlow`.
    fn status_str(status: FlowStatus) -> &'static str {
        status.as_str()
    }

    /// Marks the atomic "an effect is currently running" flag for the guard's
    /// lifetime, clearing it on drop (so a panicking effect callback — never
    /// actually reachable, `invoke_async_effect` degrades every failure to
    /// `Error { class: other }` — still cannot leave the flag stuck on).
    ///
    /// One flag per `KeelCore`, not per-task: only one step is ever admitted at
    /// a time inside a flow (the `active_flow` mutex enforces that), so exactly
    /// one effect can be "running" at once. While set, `journalTime`/
    /// `journalRandom` pass a nested clock/random read straight through instead
    /// of journaling it or touching `active_flow` — journaling it would be
    /// wrong for replay (the effect is substituted, not re-run, on a later
    /// replay, so its incidental reads never happen again) and contending for
    /// `active_flow` here would deadlock: this flag is set for the exact
    /// window `active_flow`'s lock is held across the effect's `.await`.
    struct EffectGuard<'a>(&'a AtomicBool);

    impl<'a> EffectGuard<'a> {
        fn enter(flag: &'a AtomicBool) -> Self {
            flag.store(true, Ordering::SeqCst);
            Self(flag)
        }
    }

    impl Drop for EffectGuard<'_> {
        fn drop(&mut self) {
            self.0.store(false, Ordering::SeqCst);
        }
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
    ///
    /// Tier 2 flow state (native-only): `active_flow` holds the
    /// [`FlowHandle`] between `enterFlow`/`exitFlow`. It is a *tokio* mutex
    /// (not `std::sync::Mutex`, unlike every other lock in this crate) because
    /// `executeAsync` deliberately holds its guard across the `.await` that
    /// runs a journaled step's effect — see the crate docs. `in_effect` is set
    /// for exactly that window, so a nested clock/random read from inside the
    /// running effect passes through instead of re-touching `active_flow`
    /// (which would deadlock: the same task already holds it for this step).
    #[napi(js_name = "KeelCore")]
    #[derive(Debug)]
    pub struct KeelCore {
        engine: Arc<Engine>,
        runtime: Mutex<Runtime>,
        /// True when the runtime runs on tokio's paused virtual clock (harness
        /// only). `advanceClock` is valid only on such a handle.
        paused: bool,
        active_flow: Arc<AsyncMutex<Option<FlowHandle>>>,
        in_effect: Arc<AtomicBool>,
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
            // Install OTLP export if built `--features otel` AND `KEEL_OTEL` is
            // set (a no-op otherwise; no policy is known yet, so only the env can
            // trigger it here — `configure` below tries again once
            // `telemetry.otlp_endpoint` is known). Before `Engine::new` so the
            // subscriber is up before any span is emitted.
            maybe_init_otel(&runtime, None);
            // `Engine::new` reads `tokio::time::Instant::now()`; build it inside
            // the runtime so a paused clock's epoch is anchored (see `keel-ffi`).
            let mut engine = runtime.block_on(async { Engine::new() });
            if let Some(path) = journal_path.filter(|p| !p.is_empty()) {
                attach_journal(&mut engine, &path)
                    .map_err(|e| Error::from_reason(format!("KEEL-E040: {e}")))?;
            }
            Ok(Self {
                engine: Arc::new(engine),
                runtime: Mutex::new(runtime),
                paused,
                active_flow: Arc::new(AsyncMutex::new(None)),
                in_effect: Arc::new(AtomicBool::new(false)),
            })
        }

        /// Whether a persistent journal is attached (the `scope = persistent`
        /// dev cache is live). The front end reads this to decide whether to emit
        /// `scope = "persistent"` for the LLM dev cache (cross-run replay).
        /// Live: a `configure` whose policy carries a `journal` location
        /// attaches one after the fact.
        #[napi(getter)]
        pub fn persistent(&self) -> bool {
            self.engine.journal().is_some()
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
                .map_err(|e| throw_keel_from(env, &e))?;
            // Retry OTLP init now that `telemetry.otlp_endpoint` is known — a
            // no-op if construction's env-only attempt already exported
            // (`OnceLock`).
            maybe_init_otel(
                &lock_recover(&self.runtime),
                self.engine.telemetry_otlp_endpoint().as_deref(),
            );
            Ok(())
        }

        /// Run one intercepted call synchronously. The engine drives the layer
        /// chain on this handle's runtime; `effect(attempt)` is invoked on the
        /// main thread and returns an attempt-result object. Always returns an
        /// outcome object (engine-level failures are reported *in* the outcome).
        ///
        /// Refuses (KEEL-E005) while a durable flow is open: this path runs on
        /// the bare engine, never the open [`FlowHandle`], so it would silently
        /// downgrade a journaled step to Tier 1. The front end never calls this
        /// while a flow is active (Node effects are async-only — `executeAsync`
        /// is the flow-aware path); this guard exists so a future caller cannot
        /// reintroduce the Level-0 surprise `keel-py`'s async guard was added to
        /// prevent (mirrored, direction reversed).
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
            // `try_lock`: a flow, once opened, is essentially never contended at
            // this instant from the JS thread (no effect can be admitted without
            // going through `executeAsync`'s async path first) — a contended lock
            // here still means a flow is open, so it also refuses.
            if self.active_flow.try_lock().map_or(true, |g| g.is_some()) {
                return Err(throw_keel(
                    env,
                    "KEEL-E005",
                    "synchronous execute() is not supported while a durable flow is open; \
                     Node flows route intercepted calls through executeAsync so they are \
                     journaled. This indicates a front-end bug, not a policy problem.",
                ));
            }
            let guard = lock_recover(&self.runtime);
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
        ///
        /// **Tier 2:** while a durable flow is open, the call routes through the
        /// flow's [`FlowHandle::execute_step`] instead of the bare engine, so it
        /// is journaled and replayable — this is the async `execute_step` bridge
        /// (`keel-core`'s flow module docs). `active_flow` is locked for the
        /// FULL step (claim through terminal outcome, `in_effect` set for the
        /// effect's own `.await`), which is what serializes concurrent effects
        /// inside one flow in await order (`conformance/README.md`'s ordering
        /// rule): a second concurrent call here just awaits the same async
        /// mutex, parking its task without blocking napi's runtime thread.
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
            let active_flow = Arc::clone(&self.active_flow);
            let in_effect = Arc::clone(&self.in_effect);
            // `ThreadsafeFunction` is not `Clone`; wrap it once in an `Arc` so each
            // attempt gets an owned handle. A plain `FnMut` returning an
            // owned-capture `async move` future keeps every per-attempt future
            // `'static` + `Send`, as napi's runtime requires (mirrors keel-py).
            env.spawn_future(async move {
                let effect = Arc::new(effect);
                let effect_fn = move |attempt: u32| {
                    let effect = Arc::clone(&effect);
                    let in_effect = Arc::clone(&in_effect);
                    async move {
                        let _guard = EffectGuard::enter(&in_effect);
                        invoke_async_effect(&effect, attempt).await
                    }
                };
                // Hold the flow-admission lock across the ENTIRE step — claim
                // through terminal outcome — per the ordering rule; a bare-engine
                // call (no flow open) never touches it.
                let mut guard = active_flow.lock().await;
                let outcome = if let Some(handle) = guard.as_mut() {
                    handle.execute_step(&request, effect_fn).await
                } else {
                    drop(guard);
                    engine.execute(&request, effect_fn).await
                };
                serde_json::to_value(&outcome).map_err(|e| {
                    Error::from_reason(format!("KEEL-E040: outcome not encodable: {e}"))
                })
            })
        }

        /// The deterministic per-target metrics/discovery report. Read inside the
        /// handle runtime so `clock_ms` reflects its (possibly paused) clock.
        #[napi]
        pub fn report(&self) -> Value {
            let guard = lock_recover(&self.runtime);
            guard.block_on(async { self.engine.report() })
        }

        /// Block until every event emitted so far on this handle's live NDJSON
        /// feed (`.keel/events/`, `KEEL_EVENTS`) is written and flushed to
        /// disk. A no-op when no sink is attached. The writer thread already
        /// flushes whenever its queue drains, so a long-lived process (a
        /// `keel tail`'d server) needs this for nothing — but a short-lived
        /// script (`--import keel/hook`, `keel sim`) can exit before its last
        /// few events drain on their own; the front end calls this once at
        /// process exit (`installExitFlush`) so a one-shot run's feed is
        /// always complete for `keel tail`/`keel sim` to read afterward.
        #[napi]
        pub fn flush_events(&self) {
            if let Some(sink) = self.engine.events() {
                sink.flush();
            }
        }

        /// Harness-only: advance the paused virtual clock by `ms` milliseconds.
        /// Requires a `{ paused: true }` handle (tokio panics otherwise). `u32`
        /// (a plain JS `number`) covers every virtual-clock advance the suite
        /// needs — ~49 days of milliseconds.
        #[napi]
        pub fn advance_clock(&self, ms: u32) -> Result<()> {
            if !self.paused {
                // Advancing a real-time runtime panics tokio, which would poison
                // the runtime mutex and brick the handle. Refuse precisely.
                return Err(Error::from_reason(
                    "KEEL-E040: advanceClock requires a { paused: true } handle (harness only)",
                ));
            }
            let guard = lock_recover(&self.runtime);
            guard.block_on(async move {
                tokio::time::advance(Duration::from_millis(u64::from(ms))).await;
            });
            Ok(())
        }

        /// Open (begin or resume) a Tier 2 durable flow with this identity and
        /// make it the active flow: subsequent `executeAsync` calls are
        /// journaled steps and `journalTime`/`journalRandom` virtualize reads,
        /// until `exitFlow`.
        ///
        /// Returns `{ flowId, status, replay }` — `status` is the flow's state
        /// at entry (`"completed"` ⇒ a pure replay of a finished run), `replay`
        /// is that predicate as a bool. Throws: `KEEL-E030` if another live
        /// holder leases the flow, `KEEL-E032` if it is dead, `KEEL-E040` if
        /// this core has no journal (Tier 2 requires the native core + a
        /// journal). Called on the JS thread (never from inside a spawned
        /// future), so `blocking_lock` on `active_flow` cannot deadlock here.
        #[napi]
        #[allow(
            clippy::needless_pass_by_value,
            reason = "napi decodes JS strings/options into owned args"
        )]
        pub fn enter_flow(
            &self,
            env: Env,
            entrypoint: String,
            args_hash: String,
            code_hash: Option<String>,
            explicit_key: Option<String>,
            lease_ms: Option<u32>,
        ) -> Result<Value> {
            // Read the journal LIVE from the engine: a `configure` whose policy
            // carries a `journal` location replaces the construction
            // attachment, and Tier 2 steps must land in the same store the
            // engine caches through (never a stale construction-time snapshot).
            let journal = self.engine.journal().ok_or_else(|| {
                throw_keel(
                    env,
                    "KEEL-E040",
                    "Tier 2 durable flows require a native core with a journal; this core is \
                     in-memory. Pass a journalPath (the front end attaches one under .keel/).",
                )
            })?;
            let desc = FlowDescriptor {
                entrypoint,
                args_hash,
                explicit_key,
                code_hash,
            };
            let default = FlowConfig::default();
            let config = FlowConfig {
                lease_ttl: lease_ms
                    .map_or(default.lease_ttl, |ms| Duration::from_millis(u64::from(ms))),
                max_attempts: default.max_attempts,
            };
            let holder = ProcessId::new(format!("pid-{}", std::process::id()));
            let manager = FlowManager::with_config(
                Arc::clone(&self.engine),
                journal,
                Arc::new(SystemClock),
                holder,
                config,
            );
            // Enter inside the runtime so the lease heartbeat can spawn (it
            // no-ops outside a runtime); the enter itself is synchronous
            // journal work.
            let handle = {
                let guard = lock_recover(&self.runtime);
                guard.block_on(async { manager.enter_flow(&desc) })
            }
            .map_err(|e| throw_keel_from(env, &e))?;

            let info = json!({
                "flow_id": handle.flow_id().to_string(),
                "status": status_str(handle.entry_status()),
                "replay": handle.is_replay_only(),
            });
            // Not inside a spawned future here (this method never `.await`s), so
            // no ambient tokio task context exists on this thread: `blocking_lock`
            // is a plain blocking lock, not a panic risk.
            *self.active_flow.blocking_lock() = Some(handle);
            Ok(info)
        }

        /// Close the active flow, stamping its terminal `status` (`"completed"`
        /// or `"failed"`) and aborting the lease heartbeat. A no-op if no flow
        /// is open, so the front end can call it unconditionally on scope exit.
        ///
        /// Uses `try_lock`, deliberately NOT `blocking_lock`: `exitFlow` is
        /// called on the JS thread after the front end has (or should have)
        /// `await`ed every intercepted call inside the flow, at which point
        /// `active_flow` is uncontended — but a flow body that fires an effect
        /// WITHOUT awaiting it (a bug: the ordering rule already asks callers to
        /// await sequentially) could still have a step in flight here. Were this
        /// `blocking_lock`, that step's own effect could only ever resolve its
        /// `Promise` by running JS on this SAME thread — a real deadlock, not a
        /// hypothetical one. `try_lock` instead turns that misuse into a precise
        /// `KEEL-E040` (fail loud) rather than hanging the process forever.
        #[napi]
        #[allow(
            clippy::needless_pass_by_value,
            reason = "napi decodes the JS string into an owned String"
        )]
        pub fn exit_flow(&self, env: Env, status: String) -> Result<()> {
            let final_status = match status.as_str() {
                "completed" => FlowStatus::Completed,
                "failed" => FlowStatus::Failed,
                other => {
                    return Err(throw_keel(
                        env,
                        "KEEL-E040",
                        &format!(
                            "exitFlow status must be \"completed\" or \"failed\", got {other:?}"
                        ),
                    ));
                }
            };
            let Ok(mut guard) = self.active_flow.try_lock() else {
                return Err(throw_keel(
                    env,
                    "KEEL-E040",
                    "exitFlow could not acquire the flow (an intercepted call is still in \
                     flight). Await every executeAsync/journalTime/journalRandom call inside \
                     the flow before it returns — a fired-and-forgotten effect is never \
                     journaled reliably and, here, would otherwise hang the process.",
                ));
            };
            if let Some(mut handle) = guard.take() {
                handle.complete(final_status);
                // `handle` drops here, aborting the heartbeat task.
            }
            Ok(())
        }

        /// Journal (or, on replay, substitute) a virtualized clock read under
        /// `key` (the front-end convention, e.g. `ts:Date.now#-`). Must be
        /// called inside a flow (`KEEL-E040` otherwise).
        ///
        /// **Synchronous by necessity** (`Date.now` is a synchronous JS
        /// builtin) but the flow-admission lock is async — so this uses
        /// `try_lock`, never `blocking_lock`/`.await`. A read made from
        /// *inside* a currently-running effect is never journaled (it passes
        /// through to the live value — `in_effect` is set for exactly that
        /// window); a read that genuinely races a *different*, currently
        /// in-flight step (e.g. `Date.now()` called between starting and
        /// awaiting an un-awaited `executeAsync`, or from a second racing
        /// `Promise.all` branch) ALSO passes through unjournaled rather than
        /// blocking the JS thread — blocking here could deadlock the process,
        /// since the in-flight step's own effect can only resolve its `Promise`
        /// by running JS on this same thread. This is a disclosed simplification:
        /// keep flow dispatch sequential (`await` each effect) per
        /// `conformance/README.md`'s ordering rule to get every top-level read
        /// journaled; a racing read is already nondeterminism-adjacent territory
        /// the ordering rule asks callers to avoid.
        #[napi]
        #[allow(
            clippy::needless_pass_by_value,
            reason = "napi decodes the JS string into an owned String"
        )]
        pub fn journal_time(&self, env: Env, key: String, now_ms: i64) -> Result<i64> {
            if self.in_effect.load(Ordering::SeqCst) {
                return Ok(now_ms);
            }
            let Ok(mut guard) = self.active_flow.try_lock() else {
                return Ok(now_ms); // racing a concurrently in-flight step: pass through
            };
            let handle = guard
                .as_mut()
                .ok_or_else(|| throw_keel(env, "KEEL-E040", "journalTime called outside a flow"))?;
            handle
                .journal_time(&key, now_ms)
                .map_err(|e| throw_keel_from(env, &e))
        }

        /// Journal (or substitute) a virtualized random draw under `key` (e.g.
        /// `ts:Math.random#-`). Must be inside a flow. As with [`journal_time`],
        /// a draw made inside a running effect — or racing a different
        /// in-flight step — passes through unjournaled rather than risking a
        /// same-thread deadlock (see that method's docs).
        #[napi]
        #[allow(
            clippy::needless_pass_by_value,
            reason = "napi decodes the JS string/buffer into owned args"
        )]
        pub fn journal_random(&self, env: Env, key: String, data: Vec<u8>) -> Result<Vec<u8>> {
            if self.in_effect.load(Ordering::SeqCst) {
                return Ok(data);
            }
            let Ok(mut guard) = self.active_flow.try_lock() else {
                return Ok(data); // racing a concurrently in-flight step: pass through
            };
            let handle = guard.as_mut().ok_or_else(|| {
                throw_keel(env, "KEEL-E040", "journalRandom called outside a flow")
            })?;
            handle
                .journal_random(&key, data)
                .map_err(|e| throw_keel_from(env, &e))
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
