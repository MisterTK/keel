//! The `Journal` trait: the persistence boundary the engine and flow manager
//! write through, sketched in architecture spec §4.2.
//!
//! The operation set is exactly the spec's — `begin_flow`, `record_step`,
//! `lookup_step`, `complete_flow`, `incomplete_flows`, `acquire_lease`,
//! `put_cache`, `get_cache` — adapted to idiomatic Rust: `&self` (a journal is
//! a shared handle), typed newtypes instead of bare strings, `Result` on every
//! operation (persistence can always fail), and `Duration` for the two
//! time-to-live arguments. Design constraints the trait exists to honour
//! (architecture spec §6): append-heavy, idempotent writes, and no cross-flow
//! transaction except the lease.

use core::time::Duration;

use crate::error::Result;
use crate::types::{
    CacheKey, FlowDescriptor, FlowId, FlowStatus, NewFlow, ProcessId, StepKey, StepOutcome,
};

/// A crash-durable record of flows, their steps, and the persistent response
/// cache. SQLite is the only backend in v1 ([`SqliteJournal`](crate::SqliteJournal));
/// Postgres and an object-log backend are planned behind this same trait.
pub trait Journal: Send + Sync {
    /// Open a new flow record (status `running`, no lease, timestamps stamped
    /// from the journal's clock) and return its id. Idempotent on `flow_id`:
    /// re-beginning an existing flow is a no-op that returns the same id,
    /// never a reset of its status or timestamps — so a recovering process may
    /// call it freely.
    fn begin_flow(&self, flow: &NewFlow) -> Result<FlowId>;

    /// Record (or re-record) the step at `seq`. Idempotent on `(flow_id, seq)`
    /// via upsert; the original `started_at` is preserved across re-records so
    /// a `running`→`ok` transition keeps its true start.
    fn record_step(
        &self,
        flow: &FlowId,
        seq: u64,
        key: &StepKey,
        outcome: &StepOutcome,
    ) -> Result<()>;

    /// Fetch the recorded outcome for the step whose full identity is
    /// `(flow_id, seq, key)` — the replay hit. A `seq` that exists under a
    /// *different* key is a miss here (`None`); detecting that divergence
    /// (KEEL-E031) is the flow manager's concern, not the store's.
    fn lookup_step(&self, flow: &FlowId, seq: u64, key: &StepKey) -> Result<Option<StepOutcome>>;

    /// The recorded step at `seq` regardless of its key: the replay cursor the
    /// flow manager reads to decide replay-hit vs. `(seq, step_key)` divergence
    /// (KEEL-E031). It must be consulted *before* [`record_step`](Self::record_step),
    /// which overwrites `step_key` on a `(flow_id, seq)` conflict. `None` means
    /// nothing is recorded at that seq yet (normal live progress, not a
    /// divergence); `Some((key, outcome))` carries the recorded key to compare
    /// against and the outcome to substitute on a match.
    fn step_at(&self, flow: &FlowId, seq: u64) -> Result<Option<(StepKey, StepOutcome)>>;

    /// Read one flow by id, if it exists — a status/recovery read (`keel
    /// status`, and the flow manager's dead/mode check on entry) that sits
    /// outside the recovery-scoped [`incomplete_flows`](Self::incomplete_flows).
    fn get_flow(&self, flow: &FlowId) -> Result<Option<FlowDescriptor>>;

    /// Move a flow to a terminal (or otherwise final) status, stamping
    /// `updated_at` and clearing any lease.
    fn complete_flow(&self, flow: &FlowId, status: FlowStatus) -> Result<()>;

    /// The flows still in `running` status, ordered by `flow_id`. With
    /// `lease_expired = true` these are recovery candidates (lease absent or
    /// expired against the journal's clock — safe to steal); with `false`,
    /// those still actively leased (a live-execution view).
    fn incomplete_flows(&self, lease_expired: bool) -> Result<Vec<FlowDescriptor>>;

    /// Try to take (or extend) the lease on a `running` flow for `ttl`.
    /// Succeeds — a single conditional `UPDATE` — when the lease is free,
    /// already held by `holder` (a heartbeat), or expired against the
    /// journal's clock. Returns whether this handle now holds it.
    fn acquire_lease(&self, flow: &FlowId, holder: &ProcessId, ttl: Duration) -> Result<bool>;

    /// Insert or replace a persistent cache entry, expiring `ttl` from now.
    fn put_cache(&self, key: &CacheKey, value: &[u8], ttl: Duration) -> Result<()>;

    /// Fetch a cache entry if present and not yet expired against the
    /// journal's clock; expired entries read as `None`.
    fn get_cache(&self, key: &CacheKey) -> Result<Option<Vec<u8>>>;
}
