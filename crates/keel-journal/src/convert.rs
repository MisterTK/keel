//! Backend-agnostic pieces shared by every [`Journal`](crate::Journal)
//! implementation: the raw-row shapes a backend fetches columns into, the pure
//! validation that turns them into the typed domain objects the trait returns,
//! and the two small numeric coercions every backend needs (`u64` seq → the
//! schema's signed integer domain, `Duration` → whole milliseconds).
//!
//! Keeping this logic here (rather than duplicated per backend) is what makes
//! "corrupt row" mean the same thing on SQLite and Postgres: both backends
//! fetch raw columns their own way, then hand them to the same
//! [`flow_from_row`]/[`step_from_row`] to reject values outside the frozen
//! schema's `CHECK` sets identically.

use core::time::Duration;

use crate::error::{Error, Result};
use crate::types::{
    FlowDescriptor, FlowId, FlowStatus, ProcessId, StepKind, StepOutcome, StepStatus,
    error_class_from_db,
};

/// The `flows` columns every backend selects, in the fixed order
/// [`FlowRowData`] is filled in.
pub(crate) const FLOW_COLUMNS: &str = "flow_id, entrypoint, args_hash, code_hash, status, \
     lease_holder, lease_expires, created_at, updated_at";

/// A flow row exactly as the columns arrive from the backend, before typing.
pub(crate) struct FlowRowData {
    pub(crate) flow_id: String,
    pub(crate) entrypoint: String,
    pub(crate) args_hash: String,
    pub(crate) code_hash: Option<String>,
    pub(crate) status: String,
    pub(crate) lease_holder: Option<String>,
    pub(crate) lease_expires: Option<i64>,
    pub(crate) created_at: i64,
    pub(crate) updated_at: i64,
}

/// A step row exactly as the columns arrive from the backend, before typing.
pub(crate) struct StepRowData {
    pub(crate) kind: String,
    pub(crate) attempt: i64,
    pub(crate) outcome: String,
    pub(crate) payload: Option<Vec<u8>>,
    pub(crate) error_class: Option<String>,
    pub(crate) started_at: i64,
    pub(crate) ended_at: Option<i64>,
}

/// Type and validate a raw flow row, rejecting any value outside the frozen
/// schema's `CHECK` set as [`Error::Corrupt`].
pub(crate) fn flow_from_row(raw: FlowRowData) -> Result<FlowDescriptor> {
    Ok(FlowDescriptor {
        flow_id: FlowId::new(raw.flow_id),
        entrypoint: raw.entrypoint,
        args_hash: raw.args_hash,
        code_hash: raw.code_hash,
        status: FlowStatus::from_db(&raw.status)?,
        lease_holder: raw.lease_holder.map(ProcessId::new),
        lease_expires: raw.lease_expires,
        created_at: raw.created_at,
        updated_at: raw.updated_at,
    })
}

/// Type and validate a raw step row, rejecting any value outside the frozen
/// schema's `CHECK` set as [`Error::Corrupt`].
pub(crate) fn step_from_row(raw: StepRowData) -> Result<StepOutcome> {
    let error_class = raw
        .error_class
        .as_deref()
        .map(|value| error_class_from_db("steps.error_class", value))
        .transpose()?;
    Ok(StepOutcome {
        kind: StepKind::from_db(&raw.kind)?,
        attempt: u32::try_from(raw.attempt)
            .map_err(|_| Error::corrupt("steps.attempt", raw.attempt))?,
        status: StepStatus::from_db(&raw.outcome)?,
        payload: raw.payload,
        error_class,
        started_at: raw.started_at,
        ended_at: raw.ended_at,
    })
}

/// Clamp a `Duration` to whole milliseconds as an `i64` (saturating; a TTL past
/// the epoch's `i64` range is not a real configuration).
pub(crate) fn duration_ms(ttl: Duration) -> i64 {
    i64::try_from(ttl.as_millis()).unwrap_or(i64::MAX)
}

/// Narrow a `u64` sequence number to the schema's signed integer domain.
pub(crate) fn to_i64(column: &'static str, seq: u64) -> Result<i64> {
    i64::try_from(seq).map_err(|_| Error::corrupt(column, seq))
}
