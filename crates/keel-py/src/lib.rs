//! `keel_core`: the PyO3 native module wrapping [`keel_engine::Engine`] for the
//! Keel Python front end. It exposes the same logical surface as the pure-Python
//! `keel-core-stub` (the conformance referee) so the two are interchangeable
//! behind `keel._backend`:
//!
//! - `KeelCore.configure(policy: dict)` — raises [`KeelCoreError`]`(code, message)`
//!   mapping [`KeelError`], with `.code`/`.message` attributes.
//! - `KeelCore.execute(request: dict, effect)` — synchronous. The GIL is released
//!   ([`Python::detach`]) while blocked in the engine and re-acquired
//!   ([`Python::attach`]) only to invoke the Python `effect(attempt)` callback,
//!   which returns an attempt-result dict.
//! - `KeelCore.execute_async(request, effect)` — returns an awaitable (via
//!   `pyo3-async-runtimes`); `effect(attempt)` is an `async def` awaited on the
//!   caller's asyncio loop. While a flow is open this routes through the same
//!   `FlowHandle` the synchronous path uses (see "The async flow bridge" below)
//!   instead of the bare `Engine`, so async intercepted effects inside a flow
//!   are journaled and replayable exactly like synchronous ones.
//! - `KeelCore.report()` — the deterministic report dict.
//! - `KeelCore.advance_clock(ms)` and the `KeelCore(paused=True)` flag —
//!   **harness-only** virtual-clock controls, always present but not part of the
//!   production surface.
//!
//! # Runtime & clock (mirrors `keel-ffi`)
//!
//! Each handle owns a current-thread tokio runtime (time driver only — the
//! engine uses the timer wheel, never IO). [`Engine::new`], `report()`, and clock
//! advancement all run **inside** that runtime under `block_on`, because they
//! read `tokio::time::Instant`; under `paused=True` that anchors and advances the
//! virtual clock the conformance suite drives. The runtime is behind a `Mutex`
//! so calls on one handle serialize (no `.await` is held across that std mutex —
//! `block_on` is synchronous from our side).
//!
//! The async path instead runs the engine future on the `pyo3-async-runtimes`
//! tokio runtime (real, non-paused time, and — unless configured otherwise —
//! multi-threaded, so concurrent `execute_async` calls genuinely run on separate
//! OS threads until they need the GIL) so it can await Python coroutines on the
//! event loop; the shared [`Engine`] is `&self`-concurrent, held as an `Arc`, so
//! no lock is taken across an `.await` when no flow is open.
//!
//! # The async flow bridge (concurrent admission ordering)
//!
//! `active_flow` is an `Arc<tokio::sync::Mutex<Option<FlowHandle>>>` — an
//! *async* mutex, never a blocking one, across the `execute_async` path. This is
//! the serialization point conformance/README.md's "Async steps inside a flow"
//! section specifies: `execute_async` acquires it with `.lock().await` (queuing
//! FIFO, without parking an OS thread) before touching the handle, and holds the
//! guard for the *entire* step — from `seq` admission (the first line of
//! `FlowHandle::execute_step`) through the terminal record — so two effects
//! `asyncio.gather`ed inside one flow are admitted, and therefore journaled, in
//! the order their `execute_async` calls reach the handle, never in completion
//! order. Outside a flow (`active_flow` is `None`) the lock is only held long
//! enough to observe that, so unrelated Tier 1 `execute_async` calls still run
//! fully concurrently — the serialization is a *flow* property, not a handle-wide
//! one. The **synchronous** `execute`/`enter_flow`/`exit_flow`/`journal_time`/
//! `journal_random` paths take the same lock via `blocking_lock()`, always
//! wrapped in [`Python::detach`] first: a synchronous method that blocked on this
//! mutex *while holding the GIL* could deadlock against an in-flight
//! `execute_async` step that itself needs the GIL to invoke its Python effect
//! callback (`invoke_async_effect`'s `Python::attach`) before it can release the
//! mutex.
//!
//! # Envelope translation
//!
//! One mechanism — `pythonize`/`depythonize` — bridges Python values and the
//! normative serde types in `contracts/core_api.rs` directly (no JSON-string
//! round-trip), preserving value types and serde error paths. A request dict is
//! depythonized to a [`serde_json::Value`] first so a missing `idempotent` can
//! default to `false` (matching the stub) before typing it as a [`Request`].
//! A malformed effect result degrades to `Error { class: other }` exactly as the
//! FFI facade does — the callback can never crash the core.

use std::cell::Cell;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use keel_core_api::{
    AttemptResult, BreakerState, ENVELOPE_VERSION, ErrorClass, ErrorCode, KeelError, Outcome,
    OutcomeError, Request,
};
use keel_engine::{Engine, FlowConfig, FlowDescriptor, FlowHandle, FlowManager};
use keel_journal::{FlowStatus, ProcessId, SqliteJournal, SystemClock};
use pyo3::exceptions::PyException;
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use pythonize::{depythonize, pythonize};
use serde_json::{Value, json};
use tokio::runtime::{Builder, Runtime};
use tokio::sync::Mutex as AsyncMutex;

pyo3::create_exception!(
    keel_core,
    KeelCoreError,
    PyException,
    "Keel core error. Carries the stable `code` (\"KEEL-E0NN\") and `message` as \
     attributes — the Python mirror of the core's KeelError."
);

/// Build a [`KeelCoreError`] with `.code`/`.message` attributes set on the
/// instance (so callers can read `err.code` like the stub's `KeelError`).
fn keel_error(py: Python<'_>, code: &str, message: &str) -> PyErr {
    let err = KeelCoreError::new_err(format!("{code}: {message}"));
    let value = err.value(py);
    let _ = value.setattr("code", code);
    let _ = value.setattr("message", message);
    err
}

/// Map a core [`KeelError`] to the Python exception, preserving its code.
fn keel_error_from(py: Python<'_>, err: &KeelError) -> PyErr {
    keel_error(py, err.code.as_str(), &err.message)
}

/// The `Error { class: other }` degradation for a callback that could not
/// produce a usable [`AttemptResult`] — identical to the FFI facade's rule.
fn synth_other(message: String) -> AttemptResult {
    AttemptResult::Error {
        class: ErrorClass::Other,
        http_status: None,
        retry_after_ms: None,
        message,
        original: None,
    }
}

thread_local! {
    /// `true` while a synchronous effect is executing on this OS thread.
    ///
    /// The sync `execute` path holds this handle's `runtime` (and, in a flow, its
    /// `active_flow`) mutex across a `block_on`, and the effect runs Python on the
    /// *same* thread. Anything the effect body does that re-enters this core —
    /// a nested intercepted call (a wrapped `py:` function whose body calls
    /// `requests.get`), or a patched `time.time`/`random.random` read that routes
    /// to `journal_time`/`journal_random` (e.g. `http.cookiejar` inside every
    /// `requests` response) — would otherwise re-lock a held mutex (deadlock) or
    /// start a second `block_on` on the current-thread runtime (panic). While
    /// this flag is set, those re-entrant paths *pass through* (run directly)
    /// instead of routing back through the engine/journal.
    static IN_EFFECT: Cell<bool> = const { Cell::new(false) };
}

/// Marks the current thread as "inside an effect" for the guard's lifetime,
/// restoring the previous value on drop (so nested effects nest correctly and a
/// panic in the effect cannot leave the flag stuck on).
struct InEffectGuard(bool);

impl InEffectGuard {
    fn enter() -> Self {
        Self(IN_EFFECT.replace(true))
    }
}

impl Drop for InEffectGuard {
    fn drop(&mut self) {
        IN_EFFECT.set(self.0);
    }
}

/// Whether an effect is currently running on this thread (a re-entrant call).
fn in_effect() -> bool {
    IN_EFFECT.with(Cell::get)
}

/// Lock the per-handle `runtime` mutex, recovering the guard even if a previous
/// holder panicked while holding it. The guarded runtime remains valid after an
/// unrelated panic, so one panic must not permanently brick the handle with an
/// opaque `PanicException` on every later call (poisoned-mutex lock-out).
/// `active_flow` uses a `tokio::sync::Mutex` instead (never poisoned; see the
/// module docs' "async flow bridge" section), so this only ever guards `runtime`.
fn lock_recover<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Build a terminal [`Outcome`] from a single effect attempt, applying no layer
/// chain. This is the *passthrough* a re-entrant (nested) intercepted call
/// returns: the outer call keeps full Tier 1 resilience, the inner one degrades
/// to a direct invocation rather than deadlocking. Pure — unit tested.
fn outcome_from_single_attempt(result: AttemptResult) -> Outcome {
    let mut outcome = Outcome {
        v: ENVELOPE_VERSION,
        result: String::from("error"),
        payload: None,
        error: None,
        attempts: 1,
        from_cache: false,
        waits_ms: Vec::new(),
        throttled: false,
        throttle_wait_ms: 0,
        breaker: BreakerState::Closed,
        trace_id: String::from("t-nested"),
    };
    match result {
        AttemptResult::Ok { payload } => {
            outcome.result = String::from("ok");
            outcome.payload = Some(payload);
        }
        AttemptResult::Error {
            class,
            http_status,
            message,
            original,
            ..
        } => {
            outcome.error = Some(OutcomeError {
                code: ErrorCode::NonRetryableError,
                class,
                http_status,
                message,
                original,
            });
        }
    }
    outcome
}

/// Type a request `Value` as a [`Request`], defaulting a missing `idempotent`
/// to `false` (the stub's `.get("idempotent", False)` semantics). Pure — unit
/// tested without a Python interpreter.
fn request_from_value(mut value: Value) -> Result<Request, String> {
    if let Some(map) = value.as_object_mut() {
        map.entry("idempotent").or_insert(Value::Bool(false));
    }
    serde_json::from_value(value).map_err(|e| e.to_string())
}

/// Decode a Python request dict into a [`Request`] (KEEL-E003 on failure).
fn decode_request(py: Python<'_>, request: &Bound<'_, PyAny>) -> PyResult<Request> {
    let value: Value = depythonize(request).map_err(|e| {
        keel_error(
            py,
            "KEEL-E003",
            &format!("request envelope not decodable: {e}"),
        )
    })?;
    request_from_value(value)
        .map_err(|e| keel_error(py, "KEEL-E003", &format!("request envelope invalid: {e}")))
}

/// Decode one Python attempt-result dict into an [`AttemptResult`], degrading a
/// malformed value to `Error { class: other }` (never fails the core).
fn decode_attempt(obj: &Bound<'_, PyAny>) -> AttemptResult {
    match depythonize::<AttemptResult>(obj) {
        Ok(result) => result,
        Err(e) => synth_other(format!("effect result is not a valid attempt result: {e}")),
    }
}

/// Serialize an [`Outcome`] to a Python dict (KEEL-E040 if it somehow fails).
fn outcome_to_py(py: Python<'_>, outcome: &Outcome) -> PyResult<Py<PyAny>> {
    pythonize(py, outcome)
        .map(Bound::unbind)
        .map_err(|e| keel_error(py, "KEEL-E040", &format!("outcome not encodable: {e}")))
}

/// Invoke the synchronous Python effect for one attempt (GIL held here). Any
/// Python-side error or undecodable result degrades to `Error { class: other }`.
fn invoke_sync_effect(py: Python<'_>, effect: &Py<PyAny>, attempt: u32) -> AttemptResult {
    match effect.call1(py, (attempt,)) {
        Ok(obj) => decode_attempt(obj.bind(py)),
        Err(err) => synth_other(format!("effect callback raised: {err}")),
    }
}

/// Await the asynchronous Python effect for one attempt: acquire the GIL to call
/// `effect(attempt)` and turn the returned awaitable into a Rust future, then
/// await it off the GIL and decode the result. Failures degrade to
/// `Error { class: other }`.
async fn invoke_async_effect(effect: &Py<PyAny>, attempt: u32) -> AttemptResult {
    let future = Python::attach(|py| {
        let awaitable = effect.call1(py, (attempt,))?;
        pyo3_async_runtimes::tokio::into_future(awaitable.into_bound(py))
    });
    let future = match future {
        Ok(future) => future,
        Err(err) => return synth_other(format!("async effect callback raised: {err}")),
    };
    match future.await {
        Ok(obj) => Python::attach(|py| decode_attempt(obj.bind(py))),
        Err(err) => synth_other(format!("awaiting async effect result failed: {err}")),
    }
}

/// Build a current-thread runtime with only the time driver; `paused` turns on
/// tokio's virtual clock (the conformance harness's model of time).
fn build_runtime(paused: bool) -> std::io::Result<Runtime> {
    let mut builder = Builder::new_current_thread();
    builder.enable_time();
    if paused {
        builder.start_paused(true);
    }
    builder.build()
}

/// Best-effort OTLP span + metrics export, gated by the `otel` build feature
/// AND [`keel_engine::otel::export_enabled`] (env `KEEL_OTEL` first, else the
/// effective policy's `telemetry.otlp_endpoint`). OFF by default: a wheel built
/// without `--features otel` links no OpenTelemetry dependency and this is a
/// no-op. When enabled, the FIRST call that decides to export installs the
/// global OTLP exporter — [`keel_engine::otel::resolve_endpoint`] picks the
/// endpoint (env wins over policy) — so the engine's spans/metrics reach a
/// collector. Called twice per core: once at construction with no policy known
/// yet (`policy_endpoint = None`, so only `KEEL_OTEL` + `OTEL_*` env can trigger
/// it), and again from `configure` once the effective policy's `[telemetry]` is
/// known; the `OnceLock` makes the second call a no-op if the first already
/// exported. Init failures never break the process — they warn and export
/// stays off. Best-effort: buffered spans/metrics flush as the core's runtime
/// runs (architecture spec §4.5, otel.rs).
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
        // init_otlp builds the OTLP exporters (needs a tokio runtime context) and
        // installs the global tracing subscriber exactly once per process.
        match runtime.block_on(async { keel_engine::otel::init_otlp(endpoint.as_deref()) }) {
            Ok(guard) => Some(guard),
            Err(e) => {
                eprintln!("keel: OTel export enabled but OTLP init failed ({e}); export disabled");
                None
            }
        }
    });
}

/// No-op when the `otel` feature is off (the default): no OpenTelemetry
/// dependency is linked and the core never touches telemetry.
#[cfg(not(feature = "otel"))]
fn maybe_init_otel(_runtime: &Runtime, _policy_endpoint: Option<&str>) {}

/// Open (creating the parent dir + file as needed) a WAL SQLite journal at
/// `path` on the wall clock and attach it — enabling the `scope = persistent`
/// dev cache so identical prompts replay across separate `keel run` processes
/// (Task 14 item 1). `SystemClock` is correct here: production runs on real
/// time, so cache-TTL expiry is measured in wall-clock ms.
fn attach_journal(engine: &mut Engine, path: &str) -> Result<(), String> {
    if let Some(parent) = std::path::Path::new(path).parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("journal dir {}: {e}", parent.display()))?;
    }
    let journal =
        SqliteJournal::open(path, SystemClock).map_err(|e| format!("journal open {path}: {e}"))?;
    engine.attach_journal(journal);
    Ok(())
}

/// The status token the front end reads back from `enter_flow`.
fn status_str(status: FlowStatus) -> &'static str {
    status.as_str()
}

/// The native core handle. `engine` is the shared, `&self`-concurrent kernel;
/// `runtime` (behind a `Mutex`) drives the synchronous/paused-clock paths.
///
/// Tier 2 flow state (native-only): the journal a [`FlowManager`] runs steps
/// over is read *live* from `engine.journal()` (the same store the engine
/// caches through — a `configure` whose policy carries a `journal` location
/// replaces it), and `active_flow` holds the [`FlowHandle`] between
/// `enter_flow` and `exit_flow`. While a flow is active, `execute`/`execute_async`
/// route each intercepted call through the handle's `execute_step` (journaled,
/// replayable) instead of the bare engine — the front end drives the *same*
/// `execute`/`execute_async` API either way. `active_flow` is an async mutex
/// (not `std::sync::Mutex`) specifically so `execute_async` can serialize
/// concurrent awaited effects into a deterministic admission order without
/// blocking an OS thread — see the module docs' "async flow bridge" section.
#[pyclass(module = "keel_core")]
struct KeelCore {
    engine: Arc<Engine>,
    runtime: Mutex<Runtime>,
    /// True when the runtime runs on tokio's paused virtual clock (harness only).
    /// `advance_clock` is valid only on such a handle — advancing a real-time
    /// runtime panics tokio, so we refuse it precisely instead.
    paused: bool,
    /// The flow currently open (between `enter_flow`/`exit_flow`), if any. An
    /// `Arc` so `execute_async`'s `'static` future can hold its own clone across
    /// the `.await`.
    active_flow: Arc<AsyncMutex<Option<FlowHandle>>>,
}

impl core::fmt::Debug for KeelCore {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("KeelCore")
            .field("has_journal", &self.engine.journal().is_some())
            .finish_non_exhaustive()
    }
}

#[pymethods]
impl KeelCore {
    /// Create a core. `paused=True` (harness-only) runs it on tokio's virtual
    /// clock so `advance_clock` and auto-advanced timer waits are deterministic.
    /// `journal_path` attaches a SQLite journal (production: the persistent
    /// dev-cache scope); leave it `None` (the harness default) for an in-memory
    /// core. `paused` + a journal are not combined by any caller (two clocks).
    #[new]
    #[pyo3(signature = (paused = false, journal_path = None))]
    fn new(py: Python<'_>, paused: bool, journal_path: Option<String>) -> PyResult<Self> {
        let runtime = build_runtime(paused)
            .map_err(|e| keel_error(py, "KEEL-E040", &format!("runtime build failed: {e}")))?;
        // Install OTLP export if this build enabled `--features otel` AND
        // `KEEL_OTEL` is set (a no-op otherwise; no policy is known yet, so only
        // the env can trigger it here — `configure` below tries again once
        // `telemetry.otlp_endpoint` is known). Before `Engine::new` so the
        // subscriber is up before any span is emitted.
        maybe_init_otel(&runtime, None);
        // `Engine::new` reads `tokio::time::Instant::now()`; build it inside the
        // runtime so the paused clock's epoch is anchored (see `keel-ffi`).
        let mut engine = runtime.block_on(async { Engine::new() });
        if let Some(path) = journal_path.filter(|p| !p.is_empty()) {
            attach_journal(&mut engine, &path).map_err(|e| keel_error(py, "KEEL-E040", &e))?;
        }
        Ok(Self {
            engine: Arc::new(engine),
            runtime: Mutex::new(runtime),
            paused,
            active_flow: Arc::new(AsyncMutex::new(None)),
        })
    }

    /// Whether a persistent journal is attached — the front end reads this to
    /// decide whether to emit `scope = "persistent"` for the LLM dev cache
    /// (cross-run replay). `False` for an in-memory core. Live: a `configure`
    /// whose policy carries a `journal` location attaches one after the fact.
    #[getter]
    fn persistent(&self) -> bool {
        self.engine.journal().is_some()
    }

    /// Apply a policy document (dict, per `policy.schema.json`). Raises
    /// [`KeelCoreError`]`("KEEL-E001", ...)` with the offending field path.
    fn configure(&self, py: Python<'_>, policy: &Bound<'_, PyAny>) -> PyResult<()> {
        let value: Value = depythonize(policy)
            .map_err(|e| keel_error(py, "KEEL-E001", &format!("policy not decodable: {e}")))?;
        self.engine
            .configure(&value)
            .map_err(|e| keel_error_from(py, &e))?;
        // Retry OTLP init now that `telemetry.otlp_endpoint` is known — a no-op
        // if construction's env-only attempt already exported (`OnceLock`).
        maybe_init_otel(
            &lock_recover(&self.runtime),
            self.engine.telemetry_otlp_endpoint().as_deref(),
        );
        Ok(())
    }

    /// Run one intercepted call synchronously. The GIL is released while the
    /// engine drives the layer chain and re-acquired only to invoke
    /// `effect(attempt) -> dict`. Always returns an outcome dict.
    fn execute(
        &self,
        py: Python<'_>,
        request: &Bound<'_, PyAny>,
        effect: Py<PyAny>,
    ) -> PyResult<Py<PyAny>> {
        let request = decode_request(py, request)?;
        // Re-entrant call: this `execute` is running inside another call's effect
        // on the same thread (a wrapped `py:` function whose body makes an
        // intercepted call). A nested `block_on` on the current-thread runtime
        // panics and the `runtime` mutex is already held by the outer call, so we
        // pass through — run the effect once with no layer chain. The OUTER call
        // keeps full resilience; the inner one degrades to a direct invocation
        // rather than deadlocking (a v0.1 native-core limitation; the pure-Python
        // stub composes nesting fully).
        if in_effect() {
            let attempt = invoke_sync_effect(py, &effect, 1);
            return outcome_to_py(py, &outcome_from_single_attempt(attempt));
        }
        let engine = Arc::clone(&self.engine);
        let runtime = &self.runtime;
        let active = &self.active_flow;
        // Release the GIL across the (possibly blocking) engine run; re-acquire
        // per attempt inside the effect. Holding the runtime mutex across the
        // synchronous `block_on` serializes calls on this handle. While a flow is
        // open, route the call through its `execute_step` so it is journaled and
        // replayable; otherwise run the bare engine (identical to before). The
        // effect runs under an `InEffectGuard` so any re-entrant intercepted call
        // or time/random read it triggers passes through instead of deadlocking.
        let outcome = py.detach(move || {
            let guard = lock_recover(runtime);
            // `blocking_lock` (never the async `.lock().await`, which would need
            // an executor polling us) — safe here because we already released
            // the GIL above, so we cannot deadlock an `execute_async` step that
            // needs the GIL to invoke its Python effect before releasing this
            // same lock (see the module docs' "async flow bridge" section).
            let mut flow = active.blocking_lock();
            let effect_fn = async |attempt: u32| {
                Python::attach(|py| {
                    let _in_effect = InEffectGuard::enter();
                    invoke_sync_effect(py, &effect, attempt)
                })
            };
            match flow.as_mut() {
                Some(handle) => guard.block_on(handle.execute_step(&request, effect_fn)),
                None => guard.block_on(engine.execute(&request, effect_fn)),
            }
        });
        outcome_to_py(py, &outcome)
    }

    /// Run one intercepted call asynchronously, returning an awaitable that
    /// resolves to the outcome dict. `effect(attempt)` is awaited on the caller's
    /// asyncio loop, so it may be an `async def` performing real async IO.
    ///
    /// **The async flow bridge.** While a flow is open, this routes through the
    /// active `FlowHandle` — journaled and replayable, exactly like the
    /// synchronous `execute` — instead of silently downgrading to the bare
    /// `Engine` (a Level 0 surprise the v0.1 KEEL-E005 refusal used to guard
    /// against; that refusal is retired now that the bridge is real). The
    /// admission rule (normative: conformance/README.md "Async steps inside a
    /// flow") is enforced by `active_flow`'s async mutex: a call claims the
    /// handle — and therefore its `seq` — the instant it acquires the lock, and
    /// holds it until the step's terminal outcome is recorded, so two effects an
    /// `asyncio.gather` awaits concurrently are admitted, and journaled, in the
    /// order their calls *reach* the handle (FIFO on the lock), never in
    /// completion order. Outside a flow the lock is only held long enough to see
    /// `None`, so concurrent Tier 1 `execute_async` calls are unaffected.
    fn execute_async<'py>(
        &self,
        py: Python<'py>,
        request: &Bound<'py, PyAny>,
        effect: Py<PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let request = decode_request(py, request)?;
        let engine = Arc::clone(&self.engine);
        let active_flow = Arc::clone(&self.active_flow);
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            // A plain `FnMut` returning an OWNED-capture `async move` future
            // (each attempt gets its own `Py` clone). This keeps the per-attempt
            // future `'static` + `Send`, sidestepping the HRTB "Send is not
            // general enough" that an `async` closure borrowing state triggers
            // inside `future_into_py`'s `Send` future. Passed to whichever of
            // `execute_step`/`engine.execute` below actually runs — the other
            // branch never touches it.
            let effect_fn = move |attempt: u32| {
                let effect = Python::attach(|py| effect.clone_ref(py));
                async move { invoke_async_effect(&effect, attempt).await }
            };
            // Admission: acquire the flow lock ASYNCHRONOUSLY — `.lock().await`
            // queues fairly without parking an OS thread, so a second concurrent
            // call waits on the executor, not by blocking a worker. Only the
            // `Some` arm holds the guard across the step; the `None` arm drops it
            // immediately so unrelated concurrent calls are never serialized by a
            // flow that isn't theirs.
            let mut guard = active_flow.lock().await;
            let outcome = if let Some(handle) = guard.as_mut() {
                handle.execute_step(&request, effect_fn).await
            } else {
                drop(guard);
                engine.execute(&request, effect_fn).await
            };
            Python::attach(|py| outcome_to_py(py, &outcome))
        })
    }

    /// The deterministic per-target metrics/discovery report (dict). Read inside
    /// the runtime so `clock_ms` reflects this handle's (possibly paused) clock.
    fn report(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let value = {
            let guard = lock_recover(&self.runtime);
            guard.block_on(async { self.engine.report() })
        };
        pythonize(py, &value)
            .map(Bound::unbind)
            .map_err(|e| keel_error(py, "KEEL-E040", &format!("report not encodable: {e}")))
    }

    /// Block until every event emitted so far on this handle's live NDJSON
    /// feed (`.keel/events/`, `KEEL_EVENTS`) is written and flushed to disk.
    /// A no-op when no sink is attached (Tier 1 events off). The writer
    /// thread already flushes whenever its queue drains, so a long-lived
    /// process (a `keel tail`'d server) needs this for nothing — but a
    /// short-lived script (`keel run`, `keel sim`) can exit before its last
    /// few events drain on their own; the front end calls this once at
    /// process exit (mirroring the existing discovery-flush registration)
    /// so a one-shot run's feed is always complete for `keel tail`/`keel
    /// sim` to read afterward.
    fn flush_events(&self) {
        if let Some(sink) = self.engine.events() {
            sink.flush();
        }
    }

    /// Harness-only: advance the paused virtual clock by `ms` milliseconds.
    /// Requires a `paused=True` handle. On a real-time handle we refuse with a
    /// precise `KEEL-E040` instead of letting `tokio::time::advance` panic —
    /// a panic inside `block_on` here would otherwise poison the runtime mutex
    /// and brick every subsequent call on this handle.
    fn advance_clock(&self, py: Python<'_>, ms: u64) -> PyResult<()> {
        if !self.paused {
            return Err(keel_error(
                py,
                "KEEL-E040",
                "advance_clock requires a paused=True handle (harness only); this handle runs on \
                 the real clock",
            ));
        }
        let guard = lock_recover(&self.runtime);
        guard.block_on(async move {
            tokio::time::advance(Duration::from_millis(ms)).await;
        });
        Ok(())
    }

    /// Open (begin or resume) a Tier 2 durable flow with this identity and make
    /// it the active flow: subsequent `execute` calls are journaled steps and
    /// `journal_time`/`journal_random` virtualize reads, until `exit_flow`.
    ///
    /// Returns `{"flow_id", "status", "replay"}` — `status` is the flow's state
    /// at entry (`"completed"` ⇒ a pure replay of a finished run), `replay` is
    /// that predicate as a bool. Raises `KeelCoreError`: `KEEL-E030` if another
    /// live holder leases the flow, `KEEL-E032` if it is dead, `KEEL-E040` if
    /// this core has no journal (Tier 2 requires the native core + a journal).
    #[pyo3(signature = (entrypoint, args_hash, code_hash=None, explicit_key=None, lease_ms=None))]
    fn enter_flow(
        &self,
        py: Python<'_>,
        entrypoint: String,
        args_hash: String,
        code_hash: Option<String>,
        explicit_key: Option<String>,
        lease_ms: Option<u64>,
    ) -> PyResult<Py<PyAny>> {
        // Read the journal LIVE from the engine: a `configure` whose policy
        // carries a `journal` location replaces the construction attachment,
        // and Tier 2 steps must land in the same store the engine caches
        // through (never a stale construction-time snapshot).
        let journal = self.engine.journal().ok_or_else(|| {
            keel_error(
                py,
                "KEEL-E040",
                "Tier 2 durable flows require a native core with a journal; this core is \
                 in-memory. Pass a journal_path (the front end attaches one under .keel/).",
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
            lease_ttl: lease_ms.map_or(default.lease_ttl, Duration::from_millis),
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
        // Enter inside the runtime so the lease heartbeat can spawn (it no-ops
        // outside a runtime); the enter itself is synchronous journal work.
        let handle = {
            let guard = lock_recover(&self.runtime);
            guard.block_on(async { manager.enter_flow(&desc) })
        }
        .map_err(|e| keel_error_from(py, &e))?;

        let info = json!({
            "flow_id": handle.flow_id().to_string(),
            "status": status_str(handle.entry_status()),
            "replay": handle.is_replay_only(),
        });
        // Detached: `blocking_lock` while holding the GIL could deadlock an
        // in-flight `execute_async` step that needs the GIL to invoke its
        // Python effect before it can release this same lock (module docs).
        py.detach(|| *self.active_flow.blocking_lock() = Some(handle));
        pythonize(py, &info)
            .map(Bound::unbind)
            .map_err(|e| keel_error(py, "KEEL-E040", &format!("flow info not encodable: {e}")))
    }

    /// Close the active flow, stamping its terminal `status` (`"completed"` or
    /// `"failed"`) and aborting the lease heartbeat. A no-op if no flow is open,
    /// so the front end can call it unconditionally on scope exit.
    fn exit_flow(&self, py: Python<'_>, status: &str) -> PyResult<()> {
        let final_status = match status {
            "completed" => FlowStatus::Completed,
            "failed" => FlowStatus::Failed,
            other => {
                return Err(keel_error(
                    py,
                    "KEEL-E040",
                    &format!("exit_flow status must be \"completed\" or \"failed\", got {other:?}"),
                ));
            }
        };
        // Detached for the same deadlock-avoidance reason as `enter_flow`.
        let handle = py.detach(|| self.active_flow.blocking_lock().take());
        if let Some(mut handle) = handle {
            handle.complete(final_status);
            // `handle` drops here, aborting the heartbeat task.
        }
        Ok(())
    }

    /// Journal (or, on replay, substitute) a virtualized clock read under `key`
    /// (the front-end convention, e.g. `py:time.time#-`). Must be inside a flow.
    ///
    /// A read that happens *inside* an effect (a `time.time()` call from within
    /// intercepted library code such as `http.cookiejar`, or from a wrapped
    /// function body) is NOT journaled: it passes through to the live value.
    /// Journaling it would both deadlock (the flow mutex is already held for the
    /// running step) and be wrong for replay — on replay the effect is
    /// substituted, not re-run, so its incidental time reads never happen.
    /// Only the flow's own top-level reads (between steps) become value steps.
    fn journal_time(&self, py: Python<'_>, key: &str, now_ms: i64) -> PyResult<i64> {
        if in_effect() {
            return Ok(now_ms);
        }
        // Detached: see the module docs' "async flow bridge" section — a
        // synchronous method that blocked on this lock while holding the GIL
        // could deadlock an in-flight `execute_async` step that needs the GIL to
        // invoke its Python effect before it can release this same lock.
        let result = py.detach(|| {
            let mut guard = self.active_flow.blocking_lock();
            guard
                .as_mut()
                .map(|handle| handle.journal_time(key, now_ms))
        });
        match result {
            Some(inner) => inner.map_err(|e| keel_error_from(py, &e)),
            None => Err(keel_error(
                py,
                "KEEL-E040",
                "journal_time called outside a flow",
            )),
        }
    }

    /// Journal (or substitute) a virtualized random draw under `key` (e.g.
    /// `py:random.random#-`). Must be inside a flow. As with [`journal_time`], a
    /// draw made inside a running effect passes through unjournaled.
    fn journal_random<'py>(
        &self,
        py: Python<'py>,
        key: &str,
        data: Vec<u8>,
    ) -> PyResult<Bound<'py, PyBytes>> {
        if in_effect() {
            return Ok(PyBytes::new(py, &data));
        }
        // Detached: same deadlock-avoidance reason as `journal_time`.
        let result = py.detach(|| {
            let mut guard = self.active_flow.blocking_lock();
            guard
                .as_mut()
                .map(|handle| handle.journal_random(key, data))
        });
        match result {
            Some(inner) => inner
                .map(|out| PyBytes::new(py, &out))
                .map_err(|e| keel_error_from(py, &e)),
            None => Err(keel_error(
                py,
                "KEEL-E040",
                "journal_random called outside a flow",
            )),
        }
    }
}

/// The `keel_core` extension module.
#[pymodule]
fn keel_core(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<KeelCore>()?;
    m.add("KeelCoreError", m.py().get_type::<KeelCoreError>())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        AttemptResult, ErrorClass, ErrorCode, InEffectGuard, in_effect,
        outcome_from_single_attempt, request_from_value,
    };
    use serde_json::json;

    #[test]
    fn passthrough_maps_ok_attempt() {
        let out = outcome_from_single_attempt(AttemptResult::Ok {
            payload: json!({ "n": 1 }),
        });
        assert_eq!(out.result, "ok");
        assert_eq!(out.payload, Some(json!({ "n": 1 })));
        assert_eq!(out.attempts, 1);
        assert!(out.error.is_none());
    }

    #[test]
    fn passthrough_maps_error_attempt_as_terminal() {
        let out = outcome_from_single_attempt(AttemptResult::Error {
            class: ErrorClass::Http,
            http_status: Some(500),
            retry_after_ms: None,
            message: "boom".to_owned(),
            original: None,
        });
        assert_eq!(out.result, "error");
        let err = out.error.expect("error present");
        assert_eq!(err.code, ErrorCode::NonRetryableError);
        assert_eq!(err.class, ErrorClass::Http);
        assert_eq!(err.http_status, Some(500));
        assert_eq!(out.attempts, 1);
    }

    #[test]
    fn in_effect_guard_sets_and_restores_and_nests() {
        assert!(!in_effect(), "clean thread starts outside an effect");
        {
            let _outer = InEffectGuard::enter();
            assert!(in_effect());
            {
                let _inner = InEffectGuard::enter();
                assert!(in_effect(), "nested effect stays flagged");
            }
            assert!(in_effect(), "inner drop restores to still-in-effect");
        }
        assert!(!in_effect(), "outer drop clears the flag");
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
}
