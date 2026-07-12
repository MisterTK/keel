//! Keel core API contract (contracts-v1).
//!
//! The single source of truth for the envelope types is
//! `contracts/core_api.rs`, included verbatim so the frozen contract file and
//! the compiled crate can never drift. The [`policy`] module is the shared
//! typed model of `contracts/policy.schema.json`, used by both the stub and
//! the real core so their configuration semantics cannot diverge.

pub mod policy;

include!("../../../contracts/core_api.rs");
