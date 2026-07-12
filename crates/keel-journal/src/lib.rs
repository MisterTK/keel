//! Keel's persistence layer.
//!
//! Two stores, one file format apiece:
//!
//! - The [`Journal`] trait and its [`SqliteJournal`] backend — flows, their
//!   journaled steps, leases, and the persistent response cache (architecture
//!   spec §4.2–4.3). Backed by WAL-mode SQLite against the frozen
//!   `contracts/journal.sql` schema, so `keel trace` is a query and any SQLite
//!   tool can open the file.
//! - The [`DiscoveryStore`] — per-target traffic aggregates that give
//!   `keel init`/`status`/`doctor` their evidence (DX spec §2). Its schema is
//!   this crate's own, documented on the module.
//!
//! Both take an injected [`Clock`] so every timestamp they originate is
//! deterministic under test. Errors are the crate-local [`Error`]; mapping into
//! the `KEEL-E0NN` taxonomy happens where this layer is wired into the engine.

mod clock;
mod discovery;
mod error;
mod journal;
mod sqlite;
mod types;

pub use clock::{Clock, ManualClock, SystemClock};
pub use discovery::{CallObservation, CallResult, DiscoveryStore, ObservedError, TargetStats};
pub use error::{Error, Result};
pub use journal::Journal;
pub use sqlite::SqliteJournal;
pub use types::{
    CacheKey, FlowDescriptor, FlowId, FlowStatus, NewFlow, ProcessId, StepKey, StepKind,
    StepOutcome, StepStatus,
};

/// Re-exported so callers can name error classifications without a second
/// dependency on `keel-core-api`.
pub use keel_core_api::ErrorClass;
