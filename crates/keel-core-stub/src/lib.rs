//! keel-core-stub: an in-memory fake of the keel-core surface.
//!
//! Purpose (sprint plan, Sprint 0): unblock front-end teams before the real
//! core exists. It records calls, applies *trivial but well-defined*
//! resilience semantics, and returns canned outcomes supplied by the caller's
//! effect callback. The exact semantics are specified in
//! `conformance/README.md` and are shared bit-for-bit with the Python and
//! Node stubs; the real core must pass the same conformance scenarios.
//!
//! The policy document is deserialized into the typed model in [`policy`]
//! (structs all the way down — `NonZero*` counts, newtype-parsed literals,
//! closed condition enums), so "validates" and "deserializes" are the same
//! thing and every rejection carries a precise field path.
//!
//! Simplifications relative to the real core (documented, deliberate):
//! - virtual clock: waits are recorded and advance an internal ms counter,
//!   never slept
//! - jitter is parsed but not applied (deterministic waits)
//! - breaker: consecutive-failure count mode only
//! - rate limiter: fixed windows aligned to clock zero
//! - target resolution: exact key match only (no globs), fallback to
//!   `defaults.llm` for `llm:*` targets, then `defaults.outbound`
//! - `timeout` is validated but not enforced (scenarios inject `timeout`
//!   error classes instead)

pub mod policy;
mod runtime;

pub use runtime::KeelCoreStub;
