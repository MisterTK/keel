//! Keel core API contract (contracts-v1).
//!
//! The single source of truth for the envelope types is
//! `contracts/core_api.rs`; the crate compiles the vendored crate-local copy
//! (`contract/core_api.rs`) so `cargo package` produces a self-contained
//! crate, and `build.rs` asserts on every workspace build that the copy is
//! byte-identical to the frozen contract file, so they can never drift. The
//! [`policy`] module is the shared typed model of
//! `contracts/policy.schema.json`, used by both the stub and the real core so
//! their configuration semantics cannot diverge.

pub mod policy;

include!("../contract/core_api.rs");
