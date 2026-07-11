//! Adapter-pack contract, Rust form — contracts-v1.
//!
//! See ../adapter-pack.md for semantics. Rust "packs" are compile-time
//! integrations (reqwest-middleware, tower::Layer, sqlx wrapper) but expose
//! the same four operations so `keel doctor` reports uniformly.

use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confidence {
    Pinned,
    BestEffort,
}

#[derive(Debug, Clone)]
pub struct Detection {
    pub matched: bool,
    /// e.g. "reqwest", "sqlx"
    pub name: String,
    /// crate version detected from the build, empty if unknown
    pub version: String,
    pub confidence: Confidence,
}

#[derive(Debug, Clone)]
pub struct Seam {
    /// e.g. "reqwest_middleware::ClientBuilder::with"
    pub patch_point: String,
    /// the documented upstream API this relies on
    pub upstream_api: String,
    /// printed verbatim by `keel doctor`
    pub why_stable: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetKind {
    Host,
    Function,
    Llm,
    Tool,
    Mcp,
}

#[derive(Debug, Clone)]
pub struct TargetDecl {
    /// target id or pattern, e.g. "llm:openai"
    pub pattern: String,
    pub kind: TargetKind,
    /// how `idempotent` is derived at the seam
    pub idempotency_rule: String,
    /// how `args_hash` is derived at the seam
    pub args_hash_rule: String,
}

/// The four operations every pack implements. No retry/backoff/breaker logic
/// lives here — all behavior flows through the core.
pub trait AdapterPack {
    fn detect(&self) -> Detection;
    fn seams(&self) -> Vec<Seam>;
    fn targets(&self) -> Vec<TargetDecl>;
    /// Policy fragment (keel.toml JSON form, per policy.schema.json), merged
    /// UNDER user configuration.
    fn defaults(&self) -> Value;
}
