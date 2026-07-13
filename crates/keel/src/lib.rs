//! Keel as a Rust front end.
//!
//! Rust has no import hook to hang a zero-code-change promise off of (that's
//! a dynamic-language trick `python/keel`/`node/keel` use), so the Rust
//! front end keeps the "one optional attribute" ceiling (dx-spec.md
//! invariant 1) instead: call [`init`] once from your own `main`, then mark
//! the calls you want policy-driven resilience on.
//!
//! Two seams:
//!
//! * [`wrap`] (re-exported from `keel-macros`) — `#[keel::wrap(target =
//!   "...")]` on a free `async fn` routes its body through the engine's
//!   cache → rate → breaker → timeout → retry chain. See the macro's own
//!   docs (`crates/keel-macros/src/lib.rs`) for the exact v1 scope
//!   (explicit target only, `Clone` parameters, no generics/methods).
//! * [`KeelMiddleware`] — a `reqwest_middleware::Middleware` for HTTP
//!   clients built on `reqwest`. See its module docs for the exact v1
//!   scope (exact-host targets, no glob/`llm:` mapping, no body-hash
//!   caching).
//!
//! Both share one process-wide [`keel_core::Engine`], configured from
//! `<cwd>/keel.toml` (or Level 0 defaults if absent) — mirrors
//! `python/keel`'s `install_keel`/`node/keel`'s `installKeel`, minus the
//! import-hook and adapter-detection machinery neither seam needs here.
//!
//! **Deferred** (see the session gap brief's explicit descoping): no
//! `cargo-keel` subcommand, no `syn`-based static scanner
//! (`crates/keel-cli/src/scan/rust.rs`), no `keel init --rust` wiring. A
//! Rust project today adds `keel = { path = "..." }` to `Cargo.toml` by
//! hand and calls [`init`] itself; `keel doctor`/`keel init` do not yet know
//! Rust projects exist. This is real, tracked architectural debt, not an
//! oversight — flagged for prioritization rather than assumed to be next.

mod error;
mod middleware;
mod policy;

pub use error::Error;
pub use keel_macros::wrap;
pub use middleware::KeelMiddleware;

use keel_core::Engine;
use keel_core_api::KeelError;
use std::path::Path;
use std::sync::OnceLock;

static ENGINE: OnceLock<Engine> = OnceLock::new();

/// Initialize Keel from `<current_dir>/keel.toml` (or Level 0 defaults if
/// absent). Idempotent — a second call is a no-op (mirrors
/// `python/keel`'s/`node/keel`'s idempotent `install_keel`/`installKeel`).
///
/// Call this once from your own `main` before the first `#[keel::wrap]`'d
/// call or [`KeelMiddleware`] use, so a malformed `keel.toml` fails your
/// startup loudly. If you never call it, the first wrapped call/middleware
/// invocation initializes lazily from the same file — but a load/parse
/// failure at that point silently degrades to Level 0 defaults instead of
/// surfacing a [`KeelError`], since there is no caller left to hand the
/// error to by then.
///
/// # Errors
/// `KEEL-E001` if `keel.toml` is present but unreadable or not valid TOML,
/// or if the policy it describes fails schema validation.
pub fn init() -> Result<(), KeelError> {
    init_from(std::env::current_dir().unwrap_or_default())
}

/// As [`init`], loading `keel.toml` from `dir` instead of the current
/// directory (multi-root setups; tests).
///
/// # Errors
/// See [`init`].
pub fn init_from(dir: impl AsRef<Path>) -> Result<(), KeelError> {
    if ENGINE.get().is_some() {
        return Ok(());
    }
    let policy = policy::load(dir.as_ref())?;
    let engine = Engine::new();
    engine.configure(&policy)?;
    // If another thread won the race to `set`, that engine is configured
    // from the exact same file — either outcome is the "already installed"
    // no-op this function promises.
    let _ = ENGINE.set(engine);
    Ok(())
}

/// True once an [`Engine`] has been installed, whether by an explicit
/// [`init`]/[`init_from`] call or lazily by the first wrapped call/
/// middleware invocation.
#[must_use]
pub fn is_initialized() -> bool {
    ENGINE.get().is_some()
}

fn engine() -> &'static Engine {
    ENGINE.get_or_init(|| {
        let dir = std::env::current_dir().unwrap_or_default();
        let policy = policy::load(&dir).unwrap_or_else(|_| serde_json::json!({}));
        let engine = Engine::new();
        // See `init`'s docs: a bad policy file discovered only here (no
        // explicit `init()` call) degrades to Level 0 defaults rather than
        // panicking the caller's first request.
        let _ = engine.configure(&policy);
        engine
    })
}

/// Deterministic per-target metrics/discovery report (JSON), forwarded from
/// the underlying [`keel_core::Engine::report`].
pub fn report() -> serde_json::Value {
    engine().report()
}

/// Implementation detail of the `#[keel::wrap]` macro expansion. Not part of
/// the public API — its signature changes in lockstep with `keel-macros`.
#[doc(hidden)]
pub mod __private {
    pub use serde;
    pub use serde_json;

    use crate::Error;
    use keel_core_api::{AttemptResult, ENVELOPE_VERSION, ErrorClass, Request};
    use serde::{Serialize, de::DeserializeOwned};
    use std::future::Future;

    pub async fn wrap_call<T, E, F, Fut>(
        target: &str,
        op: &str,
        idempotent: bool,
        mut make_attempt: F,
    ) -> Result<T, Error<E>>
    where
        T: Serialize + DeserializeOwned + Send + 'static,
        E: std::error::Error + Send + Sync + 'static,
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T, E>>,
    {
        let request = Request {
            v: ENVELOPE_VERSION,
            target: target.to_owned(),
            op: op.to_owned(),
            idempotent,
            args_hash: None,
        };
        let mut live_ok: Option<T> = None;
        let mut live_err: Option<E> = None;
        let outcome = crate::engine()
            .execute(&request, async |_attempt: u32| match make_attempt().await {
                Ok(value) => match serde_json::to_value(&value) {
                    Ok(payload) => {
                        live_ok = Some(value);
                        AttemptResult::Ok { payload }
                    }
                    Err(err) => AttemptResult::Error {
                        class: ErrorClass::Other,
                        http_status: None,
                        retry_after_ms: None,
                        message: format!(
                            "keel: #[keel::wrap]'d function's result failed to serialize to \
                             JSON (needed for the cache path): {err}"
                        ),
                        original: None,
                    },
                },
                Err(err) => {
                    let message = err.to_string();
                    live_err = Some(err);
                    AttemptResult::Error {
                        class: ErrorClass::Other,
                        http_status: None,
                        retry_after_ms: None,
                        message,
                        original: None,
                    }
                }
            })
            .await;

        if outcome.result == "ok" {
            if outcome.from_cache {
                let payload = outcome.payload.unwrap_or(serde_json::Value::Null);
                return serde_json::from_value(payload).map_err(|err| {
                    Error::Keel(keel_core_api::OutcomeError {
                        code: keel_core_api::ErrorCode::Internal,
                        class: ErrorClass::Other,
                        http_status: None,
                        message: format!(
                            "keel: cached payload for target {target:?} failed to deserialize: \
                             {err}"
                        ),
                        original: None,
                    })
                });
            }
            return Ok(
                live_ok.expect("engine reported a non-cached success without running the effect")
            );
        }
        match live_err {
            Some(err) => Err(Error::Original(err)),
            None => {
                Err(Error::Keel(outcome.error.expect(
                    "engine reported an error outcome without an OutcomeError",
                )))
            }
        }
    }
}
