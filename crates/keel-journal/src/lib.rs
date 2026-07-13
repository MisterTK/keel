//! Keel's persistence layer.
//!
//! Three stores:
//!
//! - The [`Journal`] trait with two backends — flows, their journaled steps,
//!   leases, and the persistent response cache (architecture spec §4.2–4.3).
//!   [`SqliteJournal`] is WAL-mode SQLite against the frozen
//!   `contracts/journal.sql` schema (so `keel trace` is a query and any SQLite
//!   tool can open the file); [`PostgresJournal`] is the Level 3/fleet
//!   equivalent (architecture spec §6) for a `journal = "postgres://…"`
//!   policy, sharing the same row shapes and semantics (see
//!   [`mod@convert`]).
//! - The [`DiscoveryStore`] — per-target traffic aggregates that give
//!   `keel init`/`status`/`doctor` their evidence (DX spec §2). Its schema is
//!   this crate's own, documented on the module.
//!
//! `SqliteJournal` takes an injected [`Clock`] so every timestamp it
//! originates is deterministic under test; `PostgresJournal` instead reads
//! every timestamp from the Postgres server's own clock (see its module doc)
//! — a fleet has no single local clock to inject. Errors are the crate-local
//! [`Error`]; mapping into the `KEEL-E0NN` taxonomy happens where this layer
//! is wired into the engine.

pub mod admin;
mod clock;
mod convert;
mod discovery;
mod error;
mod journal;
mod postgres_journal;
mod sqlite;
mod types;

pub use clock::{Clock, ManualClock, SystemClock};
pub use discovery::{
    CallObservation, CallResult, DISCOVERY_SCHEMA_VERSION, DailyStats, DiscoveryStore, MS_PER_DAY,
    ObservedError, RETENTION_DAYS, TargetStats,
};
pub use error::{Error, Result};
pub use journal::Journal;
pub use postgres_journal::PostgresJournal;
pub use sqlite::SqliteJournal;
pub use types::{
    CacheKey, FlowDescriptor, FlowId, FlowStatus, NewFlow, ProcessId, StepKey, StepKind,
    StepOutcome, StepStatus,
};

/// Re-exported so callers can name error classifications without a second
/// dependency on `keel-core-api`.
pub use keel_core_api::ErrorClass;
