//! A [`reqwest_middleware::Middleware`] that routes every request through
//! the keel-core Engine chain (cache → rate → breaker → timeout → retry),
//! target-keyed by request host — the Rust analogue of `python/keel`'s
//! `httpx_pack`/`node/keel`'s `fetch.mjs` seam.
//!
//! **v1 scope** (deliberately narrow — see the session gap brief's
//! descoping notes):
//!
//! * The target is always the exact request host string: no
//!   `host:`/URL-pattern glob resolution and no `llm:<provider>` host
//!   mapping (both are front-end-only conveniences implemented in the
//!   Python/Node packs on top of shared judgment helpers that have no Rust
//!   port yet, not core-level features).
//! * Idempotency follows the RFC 9110 safe/idempotent method set
//!   (GET/HEAD/OPTIONS/PUT/DELETE/TRACE) plus a default
//!   `(x-)idempotency-key` header, with no per-target `idempotency.header`
//!   policy override (that needs a *resolved-policy* read-back from the
//!   engine — a clean seam for a follow-up).
//! * Caching is always disabled (`args_hash: None`) since a stable cache
//!   key would need to buffer and hash the request body, which this v1
//!   does not do. Retry, the circuit breaker, the rate limiter, and
//!   per-attempt timeouts all work today — none of those need a cache key.
//! * **[`KeelMiddleware`] sends every attempt itself, via its own cloned
//!   [`reqwest::Client`], instead of delegating to the middleware chain's
//!   [`Next`].** This is not a design preference — it is a real, deliberate
//!   workaround for a compile-time wall discovered while building this:
//!   `keel_core::Engine::execute`'s effect closure, when it captures
//!   anything borrowing `Next<'_>`/`&mut Extensions` (both inherently
//!   non-`'static` — they're scoped to one `handle()` call), cannot be
//!   proven `Send` "in general" by rustc once `#[async_trait]` boxes
//!   `handle`'s returned future into `Pin<Box<dyn Future + Send>>` (needed
//!   for `Middleware` to be object-safe as `Arc<dyn Middleware>`). This
//!   reproduces with zero third-party crates involved — a minimal
//!   `Engine::execute` call from an `async move {}` block, wrapped in
//!   nothing but a `Send`-bound check, fails identically the moment the
//!   effect closure captures *any* non-`'static` data, regardless of how
//!   it's wrapped (`Arc`, `Mutex`, a manual `Box::pin`, boxing the outer
//!   future explicitly, …). Fixing it would mean changing
//!   `keel_core::Engine::execute`'s internals (out of this crate's
//!   territory, and a Tier 1 kernel change with workspace-wide blast
//!   radius) or waiting on the relevant rustc limitation. The workaround
//!   here needs the closure's captures to be fully owned/`'static`, which a
//!   cloned `reqwest::Client` satisfies (it sends independently of the
//!   `next` chain).
//!
//!   **Practical consequence:** any middleware added *after*
//!   [`KeelMiddleware`] in the `ClientBuilder` chain (i.e. closer to the
//!   transport) is never invoked, because [`KeelMiddleware`] never calls
//!   `next.run`. Add it last (closest to `.build()`), or as the only
//!   middleware.
//!
//! A real HTTP response is never turned into a middleware error, even a
//! persistently transient one (5xx/429) that exhausted every retry: the
//! last response actually received is always what's handed back, mirroring
//! `_http.deliver()`'s "Keel never turns a real HTTP response into a
//! failure" rule in the Python front end. Only a transport-level error
//! (connection reset, timeout with no response, DNS failure, …) or an
//! engine judgment made without ever attempting the call (a breaker
//! fast-fail, a rate-budget rejection) surfaces as an `Err`.

use http::Extensions;
use keel_core_api::{AttemptResult, ENVELOPE_VERSION, ErrorClass, Request as CoreRequest};
use reqwest::{Request, Response};
use reqwest_middleware::{Error as MwError, Middleware, Next, Result as MwResult};
use std::sync::{Arc, Mutex};

/// RFC 9110 §9.2.2 safe methods plus PUT/DELETE, which convention treats as
/// idempotent. Parity in spirit with `python/keel`'s `IDEMPOTENT_METHODS`
/// (POST/PATCH are deliberately absent: retryable only with an idempotency
/// key, the Level 0 hard rule).
const IDEMPOTENT_METHODS: [&str; 6] = ["GET", "HEAD", "OPTIONS", "PUT", "DELETE", "TRACE"];

/// Header names that mark an otherwise-unsafe request (POST/PATCH) as safe
/// to retry. Parity with `python/keel`'s `DEFAULT_IDEMPOTENCY_HEADERS`.
const DEFAULT_IDEMPOTENCY_HEADERS: [&str; 2] = ["idempotency-key", "x-idempotency-key"];

/// A [`Middleware`] that wraps every request in the keel-core Engine chain.
/// Owns a cloned [`reqwest::Client`] to send attempts itself (module docs
/// explain why: `Next`/`Extensions` cannot cross into
/// [`keel_core::Engine::execute`]'s effect closure under `#[async_trait]`'s
/// `Send`-boxing requirement).
///
/// Add it as the **last** middleware (or the only one) so no
/// transport-adjacent middleware after it is skipped:
///
/// ```no_run
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// keel::init()?;
/// let raw = reqwest::Client::new();
/// let client = reqwest_middleware::ClientBuilder::new(raw.clone())
///     .with(keel::KeelMiddleware::new(raw))
///     .build();
/// let resp = client.get("https://api.example.com/orders").send().await?;
/// # let _ = resp;
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct KeelMiddleware {
    client: reqwest::Client,
}

impl KeelMiddleware {
    /// `client` is the [`reqwest::Client`] used to actually send every
    /// attempt (cloning a `reqwest::Client` is cheap — it's an `Arc`
    /// internally — so passing the same client you built the
    /// `ClientWithMiddleware` from is the normal usage).
    #[must_use]
    pub fn new(client: reqwest::Client) -> Self {
        Self { client }
    }
}

/// Owned, `'static`-compatible, cheaply-`clone`-able handles to the state
/// one `handle()` call shares across every retry attempt and the final
/// delivery step (module docs on `handle` explain why these must be `Arc`
/// handles rather than borrowed references).
#[derive(Clone)]
struct LiveState {
    pending: Arc<Mutex<Option<Request>>>,
    ok: Arc<Mutex<Option<Response>>>,
    transient: Arc<Mutex<Option<Response>>>,
    err: Arc<Mutex<Option<MwError>>>,
}

impl LiveState {
    fn new(req: Request) -> Self {
        Self {
            pending: Arc::new(Mutex::new(Some(req))),
            ok: Arc::new(Mutex::new(None)),
            transient: Arc::new(Mutex::new(None)),
            err: Arc::new(Mutex::new(None)),
        }
    }
}

/// One retry attempt: get a sendable copy of the pending request, send it via
/// `client` directly (module docs: not `next.run`), and classify the result.
async fn run_attempt(client: reqwest::Client, live: LiveState) -> AttemptResult {
    let attempt_req = match take_attempt_request(&live.pending) {
        Ok(req) => req,
        Err(err) => return err,
    };
    match client.execute(attempt_req).await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            if is_transient_status(status) {
                let retry_after_ms = parse_retry_after_ms(resp.headers());
                let message = format!("http {status}");
                *live.transient.lock().expect("keel: mutex poisoned") = Some(resp);
                AttemptResult::Error {
                    class: ErrorClass::Http,
                    http_status: Some(status),
                    retry_after_ms,
                    message,
                    original: None,
                }
            } else {
                *live.ok.lock().expect("keel: mutex poisoned") = Some(resp);
                AttemptResult::Ok {
                    payload: serde_json::Value::Null,
                }
            }
        }
        Err(err) => {
            let class = classify(&err);
            let message = err.to_string();
            *live.err.lock().expect("keel: mutex poisoned") = Some(err.into());
            AttemptResult::Error {
                class,
                http_status: None,
                retry_after_ms: None,
                message,
                original: None,
            }
        }
    }
}

/// A clone of the pending request if its body is clonable, else the
/// original (taken, leaving `None` behind) on the very first use, else an
/// `AttemptResult::Error` for a policy-forced retry of a non-clonable body
/// (only reachable if the front end judged a streaming-body call idempotent
/// anyway — Keel does not retry non-idempotent calls, so this is a
/// defensive fallback, not the common path).
fn take_attempt_request(pending: &Mutex<Option<Request>>) -> Result<Request, AttemptResult> {
    let mut guard = pending.lock().expect("keel: pending mutex poisoned");
    if let Some(cloned) = guard.as_ref().and_then(Request::try_clone) {
        return Ok(cloned);
    }
    guard.take().ok_or_else(|| AttemptResult::Error {
        class: ErrorClass::Other,
        http_status: None,
        retry_after_ms: None,
        message: "keel: request body cannot be cloned for a retry attempt".to_owned(),
        original: None,
    })
}

/// After the engine chain runs: an `ok` outcome delivers the live response;
/// an `error` outcome still delivers a real (if unhappy) transient response
/// if one was ever received (module docs: never turn a real HTTP response
/// into a middleware error); otherwise the live transport error, or — for a
/// judgment the engine made without ever calling `run_attempt` (a breaker
/// fast-fail, a rate-budget rejection) — a synthetic error carrying that
/// judgment.
fn deliver(outcome: &keel_core_api::Outcome, live: &LiveState) -> MwResult<Response> {
    if outcome.result == "ok" {
        let resp = live
            .ok
            .lock()
            .expect("keel: mutex poisoned")
            .take()
            .expect("keel: ok outcome without a live response");
        return Ok(resp);
    }
    if let Some(resp) = live.transient.lock().expect("keel: mutex poisoned").take() {
        return Ok(resp);
    }
    if let Some(err) = live.err.lock().expect("keel: mutex poisoned").take() {
        return Err(err);
    }
    let outcome_error = outcome
        .error
        .clone()
        .expect("engine reported an error outcome without an OutcomeError");
    Err(MwError::middleware(
        crate::Error::<std::convert::Infallible>::Keel(outcome_error),
    ))
}

#[async_trait::async_trait]
impl Middleware for KeelMiddleware {
    async fn handle(
        &self,
        req: Request,
        _extensions: &mut Extensions,
        _next: Next<'_>,
    ) -> MwResult<Response> {
        let method = req.method().clone();
        let host = req.url().host_str().unwrap_or("unknown").to_owned();
        let op = format!("{method} {host}{}", req.url().path());
        let idempotent = is_idempotent(method.as_str(), req.headers());
        let core_req = CoreRequest {
            v: ENVELOPE_VERSION,
            target: host,
            op,
            idempotent,
            args_hash: None,
        };

        let client = self.client.clone();
        let live = LiveState::new(req);

        let outcome = crate::engine()
            .execute(&core_req, {
                let live = live.clone();
                move |_attempt: u32| run_attempt(client.clone(), live.clone())
            })
            .await;

        deliver(&outcome, &live)
    }
}

fn is_idempotent(method: &str, headers: &reqwest::header::HeaderMap) -> bool {
    if IDEMPOTENT_METHODS.contains(&method) {
        return true;
    }
    headers
        .keys()
        .any(|name| DEFAULT_IDEMPOTENCY_HEADERS.contains(&name.as_str()))
}

fn is_transient_status(status: u16) -> bool {
    status == 429 || (500..=599).contains(&status)
}

/// Server-provided backoff override (`Retry-After`, seconds form only — the
/// HTTP-date form is not parsed in this v1; an unparsed header just means
/// the schedule's own wait applies instead of a server-driven override).
fn parse_retry_after_ms(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    let value = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    let secs: u64 = value.trim().parse().ok()?;
    Some(secs.saturating_mul(1000))
}

fn classify(err: &reqwest::Error) -> ErrorClass {
    if err.is_timeout() {
        ErrorClass::Timeout
    } else if err.is_connect() {
        ErrorClass::Conn
    } else {
        ErrorClass::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idempotent_methods() {
        for m in ["GET", "HEAD", "OPTIONS", "PUT", "DELETE", "TRACE"] {
            assert!(is_idempotent(m, &reqwest::header::HeaderMap::new()), "{m}");
        }
        assert!(!is_idempotent("POST", &reqwest::header::HeaderMap::new()));
        assert!(!is_idempotent("PATCH", &reqwest::header::HeaderMap::new()));
    }

    #[test]
    fn post_with_idempotency_key_header_is_idempotent() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("idempotency-key", "abc".parse().unwrap());
        assert!(is_idempotent("POST", &headers));
    }

    #[test]
    fn transient_status() {
        assert!(is_transient_status(429));
        assert!(is_transient_status(500));
        assert!(is_transient_status(503));
        assert!(!is_transient_status(404));
        assert!(!is_transient_status(200));
        assert!(!is_transient_status(301));
    }

    #[test]
    fn retry_after_seconds_form() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "2".parse().unwrap());
        assert_eq!(parse_retry_after_ms(&headers), Some(2000));
    }

    #[test]
    fn retry_after_http_date_form_is_unparsed() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::RETRY_AFTER,
            "Wed, 21 Oct 2026 07:28:00 GMT".parse().unwrap(),
        );
        assert_eq!(parse_retry_after_ms(&headers), None);
    }
}
