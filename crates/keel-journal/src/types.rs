//! The typed vocabulary of the journal: identifier newtypes, the closed enums
//! that mirror the schema's `CHECK` sets, and the flow/step records that cross
//! the [`Journal`](crate::Journal) boundary.
//!
//! Enum string mappings are pinned to `contracts/journal.sql` verbatim
//! (`running`/`completed`/`failed`/`dead`, `effect`/`time`/…, `ok`/`error`/
//! `running`). A value the schema's `CHECK` would reject is
//! [`Error::Corrupt`](crate::Error::Corrupt) on read, never a silent default.

use crate::error::{Error, Result};
use keel_core_api::ErrorClass;
use serde::Serialize;

/// Declares a transparent `String` newtype with the ergonomics every
/// identifier here needs: build from anything string-like, borrow as `&str`,
/// print as itself.
macro_rules! string_id {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Wrap an owned or borrowed string as this identifier.
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            /// Borrow the underlying string.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl core::fmt::Display for $name {
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }
    };
}

string_id! {
    /// A flow execution's storage key: the ULID in `flows.flow_id`. Flow
    /// *identity* is `(entrypoint, args_hash, explicit_key?)` (architecture
    /// spec §4.3); mapping that identity to a ULID is the flow manager's job,
    /// so the journal simply stores and keys by the id it is handed.
    FlowId
}

string_id! {
    /// A step's identity within its flow: `"(target)#(args_hash)"`, matching
    /// `steps.step_key`.
    StepKey
}

string_id! {
    /// A persistent cache entry's key: `"(target)#(args_hash)"`, matching
    /// `cache.key`.
    CacheKey
}

string_id! {
    /// The holder of a flow lease — a process identity such as
    /// `"host-a:pid-4242"`, stored in `flows.lease_holder`.
    ProcessId
}

/// A flow's lifecycle status (`flows.status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FlowStatus {
    /// In flight (or crashed mid-flight and awaiting recovery).
    Running,
    /// Ran to completion.
    Completed,
    /// Terminated on a non-retryable failure.
    Failed,
    /// Failed on every resume up to the cap; surfaced by `keel flows --dead`.
    Dead,
}

impl FlowStatus {
    /// The exact token stored in `flows.status`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Dead => "dead",
        }
    }

    pub(crate) fn from_db(value: &str) -> Result<Self> {
        match value {
            "running" => Ok(Self::Running),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "dead" => Ok(Self::Dead),
            other => Err(Error::corrupt("flows.status", other)),
        }
    }
}

/// What a journaled step represents (`steps.kind`). Time and random reads are
/// effects too — the front ends virtualize them into the journal so replay is
/// deterministic (architecture spec §4.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    /// An intercepted side effect (an HTTP call, a query, a wrapped function).
    Effect,
    /// A virtualized clock read (`time.time()`, `Date.now()`).
    Time,
    /// A virtualized random draw.
    Random,
    /// A subprocess, treated as one opaque step in v1.
    Subprocess,
    /// A control marker (e.g. a replay-branch record).
    Marker,
}

impl StepKind {
    /// The exact token stored in `steps.kind`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Effect => "effect",
            Self::Time => "time",
            Self::Random => "random",
            Self::Subprocess => "subprocess",
            Self::Marker => "marker",
        }
    }

    pub(crate) fn from_db(value: &str) -> Result<Self> {
        match value {
            "effect" => Ok(Self::Effect),
            "time" => Ok(Self::Time),
            "random" => Ok(Self::Random),
            "subprocess" => Ok(Self::Subprocess),
            "marker" => Ok(Self::Marker),
            other => Err(Error::corrupt("steps.kind", other)),
        }
    }
}

/// A step's terminal state (`steps.outcome`). `Running` marks a step that
/// started but has no result yet — the shape a crash leaves behind and what
/// recovery re-executes live.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    /// Completed successfully; `payload` holds the recorded result.
    Ok,
    /// Failed; `error_class` holds the classification.
    Error,
    /// Started, not yet finished (in flight or interrupted).
    Running,
}

impl StepStatus {
    /// The exact token stored in `steps.outcome`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "error",
            Self::Running => "running",
        }
    }

    pub(crate) fn from_db(value: &str) -> Result<Self> {
        match value {
            "ok" => Ok(Self::Ok),
            "error" => Ok(Self::Error),
            "running" => Ok(Self::Running),
            other => Err(Error::corrupt("steps.outcome", other)),
        }
    }
}

/// Everything needed to open a new flow record. Identity fields plus the
/// caller-chosen `flow_id`; [`begin_flow`](crate::Journal::begin_flow) stamps
/// `status = running`, clears the lease, and sets `created_at`/`updated_at`
/// from its [`Clock`](crate::Clock) — so those are not fields here (parse,
/// don't check: a `NewFlow` cannot express an already-completed flow).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewFlow {
    /// The ULID storage key for this flow.
    pub flow_id: FlowId,
    /// The flow's entrypoint, e.g. `"py:pipeline.ingest:main"`.
    pub entrypoint: String,
    /// Hash of the flow's arguments; part of its identity.
    pub args_hash: String,
    /// Hash of the flow's code, used to fence replay across deploys.
    pub code_hash: Option<String>,
}

/// A full flow row, as read back by [`incomplete_flows`](crate::Journal::incomplete_flows)
/// and `get_flow`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FlowDescriptor {
    /// The ULID storage key.
    pub flow_id: FlowId,
    /// The flow's entrypoint.
    pub entrypoint: String,
    /// Hash of the flow's arguments.
    pub args_hash: String,
    /// Hash of the flow's code, when recorded.
    pub code_hash: Option<String>,
    /// Lifecycle status.
    pub status: FlowStatus,
    /// Current lease holder, if leased.
    pub lease_holder: Option<ProcessId>,
    /// Lease expiry (ms since epoch), if leased.
    pub lease_expires: Option<i64>,
    /// When the flow began (ms since epoch).
    pub created_at: i64,
    /// When the flow row last changed (ms since epoch).
    pub updated_at: i64,
}

/// The recorded outcome of a step — the payload the flow manager substitutes
/// on replay, plus the metadata `keel trace` renders.
///
/// The timestamps are carried, not stamped by the store: whoever observed the
/// effect (holding the same injected [`Clock`](crate::Clock)) knows when it
/// started and ended, and a `Running` step legitimately has no `ended_at`.
/// This lets [`record_step`](crate::Journal::record_step) reproduce any step
/// shape faithfully, including the crashed-mid-step shape recovery looks for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StepOutcome {
    /// What the step represents.
    pub kind: StepKind,
    /// Attempts consumed by this step (policy retries are attempts of *one*
    /// step, per architecture spec §4.3).
    pub attempt: u32,
    /// Terminal state.
    pub status: StepStatus,
    /// The recorded result (MessagePack, schema-tagged), opaque to the store.
    pub payload: Option<Vec<u8>>,
    /// Error classification when `status == Error`.
    pub error_class: Option<ErrorClass>,
    /// When the step started (ms since epoch).
    pub started_at: i64,
    /// When the step ended (ms since epoch); `None` while running.
    pub ended_at: Option<i64>,
}

/// Maps `ErrorClass` to the token stored in the journal. Kept identical to the
/// contract's own snake_case serialization so a hand-written SQL query and the
/// typed API agree on what a class looks like on disk.
pub(crate) fn error_class_str(class: ErrorClass) -> &'static str {
    match class {
        ErrorClass::Conn => "conn",
        ErrorClass::Timeout => "timeout",
        ErrorClass::Http => "http",
        ErrorClass::Cancelled => "cancelled",
        ErrorClass::Other => "other",
    }
}

/// Inverse of [`error_class_str`]; a token outside the set is corruption.
pub(crate) fn error_class_from_db(column: &'static str, value: &str) -> Result<ErrorClass> {
    match value {
        "conn" => Ok(ErrorClass::Conn),
        "timeout" => Ok(ErrorClass::Timeout),
        "http" => Ok(ErrorClass::Http),
        "cancelled" => Ok(ErrorClass::Cancelled),
        "other" => Ok(ErrorClass::Other),
        other => Err(Error::corrupt(column, other)),
    }
}
