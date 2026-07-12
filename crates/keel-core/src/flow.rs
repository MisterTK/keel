//! Tier 2: the durable flow manager (architecture spec §4.3–4.4).
//!
//! A *flow* is a function whose intercepted effects are journaled as it runs, so
//! that a crashed run can be re-executed from the top and each already-completed
//! effect is *substituted* from the journal instead of re-invoked. This is the
//! DBOS-style coordinator-free recovery model: no scheduler service exists, a
//! process just scans for incomplete flows with expired leases and resumes them.
//!
//! Two durability boundaries meet here and must not contaminate each other
//! (spec §4.3): retries *within* a step are the Tier 1 engine's business
//! (attempts of one step, journaled as that step's `attempt` count);
//! re-execution *of the flow* is this manager's business. So a
//! [`FlowHandle::execute_step`] wraps [`Engine::execute`] — the step gets full
//! Tier 1 resilience — and journals the single post-retry outcome.
//!
//! At-least-once honesty: a live step is journaled `running` *before* the effect
//! runs and its terminal outcome recorded *before* the result is released, so a
//! crash between those points leaves a `running` step that resume re-executes.
//!
//! Replay correctness is checked, not assumed (spec §4.4): the `(seq, step_key)`
//! encountered on replay must match the journal. A `seq` recorded under a
//! *different* key is nondeterminism ([`ErrorCode::FlowNondeterminism`],
//! KEEL-E031), handled per the `flows.on_nondeterminism` policy.

use core::time::Duration;
use std::sync::Arc;

use keel_core_api::policy::NondeterminismResponse;
use keel_core_api::{
    ENVELOPE_VERSION, ErrorClass, ErrorCode, KeelError, Outcome, OutcomeError, Request,
};
use keel_journal::{
    Clock, FlowId, FlowStatus, Journal, NewFlow, ProcessId, StepKey, StepKind, StepOutcome,
    StepStatus,
};
use serde_json::{Value, json};
use tracing::{debug, warn};

/// Where a `branch`-mode fresh attempt writes its steps, high above any real
/// step count so the abandoned run's records (seqs `1..`) are preserved for
/// audit (spec §4.4).
const BRANCH_SEQ_BASE: u64 = 1_000_000;

/// The reserved step slot holding the flow-level resume counter. Real steps are
/// numbered from 1, so seq 0 never collides with them.
const ATTEMPT_SEQ: u64 = 0;
/// The step key of the reserved resume-counter marker.
const ATTEMPT_KEY: &str = "flow:attempt";
/// The step key virtualized clock reads journal under (spec §4.4).
const TIME_KEY: &str = "keel:time#-";
/// The step key virtualized random draws journal under.
const RANDOM_KEY: &str = "keel:random#-";

use crate::engine::Engine;

/// How a flow is identified and fenced. Identity is
/// `(entrypoint, args_hash, explicit_key?)` (spec §4.3); `code_hash` fences
/// replay across deploys (spec §4.4).
#[derive(Debug, Clone)]
pub struct FlowDescriptor {
    /// The flow's entrypoint, e.g. `"py:pipeline.ingest:main"`.
    pub entrypoint: String,
    /// Hash of the flow's arguments; part of its identity.
    pub args_hash: String,
    /// An optional explicit identity key, disambiguating two runs that share an
    /// entrypoint and args (spec §4.3).
    pub explicit_key: Option<String>,
    /// Hash of the flow's code, used to fence replay across deploys.
    pub code_hash: Option<String>,
}

impl FlowDescriptor {
    /// The deterministic storage key for this identity. Deterministic (no ULID
    /// clock/random draw) so a rerun with the same identity keys the same flow
    /// row — which is what makes resume-on-rerun work through the idempotent
    /// [`Journal::begin_flow`].
    #[must_use]
    pub fn flow_id(&self) -> FlowId {
        FlowId::new(format!(
            "{}#{}#{}",
            self.entrypoint,
            self.args_hash,
            self.explicit_key.as_deref().unwrap_or("")
        ))
    }
}

/// Tuning for a [`FlowManager`]: lease lifetime and the flow-level attempt cap
/// (spec §4.3 leases; the cap turns a poison flow `dead`).
#[derive(Debug, Clone, Copy)]
pub struct FlowConfig {
    /// How long an acquired lease is valid before another process may steal it.
    pub lease_ttl: Duration,
    /// Maximum flow-level (re-)execution attempts before the flow is marked
    /// `dead` and refused (KEEL-E032). Distinct from Tier 1 step attempts.
    pub max_attempts: u32,
}

impl Default for FlowConfig {
    fn default() -> Self {
        Self {
            lease_ttl: Duration::from_secs(30),
            max_attempts: 3,
        }
    }
}

/// The Tier 2 flow manager: opens and resumes durable flows over a [`Journal`],
/// running each step's effect through the Tier 1 [`Engine`]. One per process,
/// `&self`-concurrent; hands out a `&mut` [`FlowHandle`] per running flow.
pub struct FlowManager {
    engine: Arc<Engine>,
    journal: Arc<dyn Journal>,
    clock: Arc<dyn Clock>,
    holder: ProcessId,
    config: FlowConfig,
}

impl core::fmt::Debug for FlowManager {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FlowManager")
            .field("holder", &self.holder)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl FlowManager {
    /// A manager with the default [`FlowConfig`].
    #[must_use]
    pub fn new(
        engine: Arc<Engine>,
        journal: Arc<dyn Journal>,
        clock: Arc<dyn Clock>,
        holder: ProcessId,
    ) -> Self {
        Self::with_config(engine, journal, clock, holder, FlowConfig::default())
    }

    /// A manager with explicit tuning.
    #[must_use]
    pub fn with_config(
        engine: Arc<Engine>,
        journal: Arc<dyn Journal>,
        clock: Arc<dyn Clock>,
        holder: ProcessId,
        config: FlowConfig,
    ) -> Self {
        Self {
            engine,
            journal,
            clock,
            holder,
            config,
        }
    }

    /// Enter (begin or resume) the flow with this identity: open the row
    /// (idempotent), take the lease, and hand back a handle in replay-or-live
    /// mode. `Err` when the lease is held by a live holder (KEEL-E030) or the
    /// flow is already `dead` (KEEL-E032).
    ///
    /// # Errors
    /// - `KEEL-E030` if another holder's lease is still valid.
    /// - `KEEL-E032` if the flow is `dead` (never auto-resumed).
    /// - `KEEL-E040` if the journal cannot open the flow row.
    pub fn enter_flow(&self, desc: &FlowDescriptor) -> Result<FlowHandle, KeelError> {
        self.enter(
            &desc.flow_id(),
            &desc.entrypoint,
            &desc.args_hash,
            desc.code_hash.as_deref(),
        )
    }

    /// Resume a flow the recovery scan surfaced ([`Journal::incomplete_flows`]),
    /// fencing against the currently-deployed `code_hash`.
    ///
    /// # Errors
    /// As [`enter_flow`](Self::enter_flow).
    pub fn resume_flow(
        &self,
        flow: &keel_journal::FlowDescriptor,
        current_code_hash: Option<&str>,
    ) -> Result<FlowHandle, KeelError> {
        self.enter(
            &flow.flow_id,
            &flow.entrypoint,
            &flow.args_hash,
            current_code_hash,
        )
    }

    fn enter(
        &self,
        flow_id: &FlowId,
        entrypoint: &str,
        args_hash: &str,
        current_code_hash: Option<&str>,
    ) -> Result<FlowHandle, KeelError> {
        self.journal
            .begin_flow(&NewFlow {
                flow_id: flow_id.clone(),
                entrypoint: entrypoint.to_owned(),
                args_hash: args_hash.to_owned(),
                code_hash: current_code_hash.map(str::to_owned),
            })
            .map_err(|e| internal(format!("begin_flow failed: {e}")))?;

        let existing = self
            .journal
            .get_flow(flow_id)
            .map_err(|e| internal(format!("get_flow failed: {e}")))?;
        let status = existing.as_ref().map_or(FlowStatus::Running, |f| f.status);

        // A dead flow is never auto-resumed (spec §4.3).
        if status == FlowStatus::Dead {
            return Err(KeelError {
                code: ErrorCode::FlowDead,
                message: format!("flow {flow_id} is dead; refusing to resume (KEEL-E032)"),
            });
        }

        // Flow-level attempt cap (distinct from Tier 1 step attempts): each
        // resume of a not-yet-completed flow consumes one attempt from the
        // reserved seq-0 counter; exceeding the cap marks the flow dead. A
        // completed flow's re-entry is pure replay and consumes nothing.
        let attempt = if status == FlowStatus::Completed {
            None
        } else {
            let prior = self
                .journal
                .step_at(flow_id, ATTEMPT_SEQ)
                .map_err(|e| internal(format!("attempt lookup failed: {e}")))?
                .map_or(0, |(_, o)| o.attempt);
            let attempt = prior.saturating_add(1);
            if attempt > self.config.max_attempts {
                self.journal
                    .complete_flow(flow_id, FlowStatus::Dead)
                    .map_err(|e| internal(format!("mark-dead failed: {e}")))?;
                return Err(KeelError {
                    code: ErrorCode::FlowDead,
                    message: format!(
                        "flow {flow_id} exceeded its {} attempt cap; marked dead (KEEL-E032)",
                        self.config.max_attempts
                    ),
                });
            }
            Some(attempt)
        };

        // A cleanly-failed flow is put back to `running` before re-leasing so a
        // resume can proceed (and so a re-crash lands it in the recovery scan).
        if status == FlowStatus::Failed {
            self.journal
                .complete_flow(flow_id, FlowStatus::Running)
                .map_err(|e| internal(format!("reset-to-running failed: {e}")))?;
        }

        let acquired = self
            .journal
            .acquire_lease(flow_id, &self.holder, self.config.lease_ttl)
            .map_err(|e| internal(format!("acquire_lease failed: {e}")))?;
        if !acquired {
            return Err(KeelError {
                code: ErrorCode::FlowLeaseHeld,
                message: format!(
                    "flow {flow_id} is leased by another holder; not resuming (KEEL-E030)"
                ),
            });
        }

        if let Some(attempt) = attempt {
            let now = self.clock.now_ms();
            let marker = StepOutcome {
                kind: StepKind::Marker,
                attempt,
                status: StepStatus::Ok,
                payload: None,
                error_class: None,
                started_at: now,
                ended_at: Some(now),
            };
            if let Err(e) =
                self.journal
                    .record_step(flow_id, ATTEMPT_SEQ, &StepKey::new(ATTEMPT_KEY), &marker)
            {
                warn!(flow = %flow_id, error = %e, "attempt-counter record failed");
            }
        }

        // Replay is fenced when the recorded code differs from what is running
        // now: a divergence under a changed deploy downgrades fail→warn (§4.4).
        let code_hash_fenced = match (existing.and_then(|f| f.code_hash), current_code_hash) {
            (Some(recorded), Some(current)) => recorded != current,
            _ => false,
        };

        let heartbeat = spawn_heartbeat(
            Arc::clone(&self.journal),
            flow_id.clone(),
            self.holder.clone(),
            self.config.lease_ttl,
        );

        Ok(FlowHandle {
            engine: Arc::clone(&self.engine),
            journal: Arc::clone(&self.journal),
            clock: Arc::clone(&self.clock),
            flow_id: flow_id.clone(),
            seq: 0,
            code_hash_fenced,
            replay_abandoned: false,
            completed: false,
            heartbeat,
        })
    }
}

/// A single running flow. Owns the step cursor (`seq`), so it is used by one
/// task via `&mut`; the manager stays `&self`-concurrent for many flows.
///
/// Dropping a handle without [`complete`](Self::complete) leaves the flow
/// `running` with its lease — exactly the crash shape recovery resumes.
pub struct FlowHandle {
    engine: Arc<Engine>,
    journal: Arc<dyn Journal>,
    clock: Arc<dyn Clock>,
    flow_id: FlowId,
    seq: u64,
    code_hash_fenced: bool,
    replay_abandoned: bool,
    completed: bool,
    /// The lease-renewal task; aborted when the handle drops so a completed or
    /// crashed flow stops heart-beating.
    heartbeat: Option<tokio::task::AbortHandle>,
}

impl core::fmt::Debug for FlowHandle {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FlowHandle")
            .field("flow_id", &self.flow_id)
            .field("seq", &self.seq)
            .field("completed", &self.completed)
            .finish_non_exhaustive()
    }
}

/// What resolving a step against the journal decided.
enum StepPlan {
    /// Run the effect live and journal it (fresh progress or a re-executed
    /// crashed step).
    Live,
    /// Substitute this recorded outcome without invoking the effect.
    Replay(StepOutcome),
    /// The recorded key at this seq differs from the current one: nondeterminism.
    Diverged { recorded: StepKey },
}

/// The context of a `warn`/`branch` divergence recovery, grouped so the
/// re-execution helper stays within argument bounds.
struct Divergence<'a> {
    seq: u64,
    recorded: &'a StepKey,
    observed: &'a StepKey,
    mode: &'static str,
    preserve: bool,
}

impl FlowHandle {
    /// This flow's storage id.
    #[must_use]
    pub fn flow_id(&self) -> &FlowId {
        &self.flow_id
    }

    fn now(&self) -> i64 {
        self.clock.now_ms()
    }

    /// Decide how the step at `seq` (with `key`) resolves against the journal.
    fn plan_step(&self, seq: u64, key: &StepKey) -> StepPlan {
        if self.replay_abandoned {
            return StepPlan::Live;
        }
        match self.journal.step_at(&self.flow_id, seq) {
            Ok(None) => StepPlan::Live,
            Ok(Some((recorded_key, outcome))) => {
                if recorded_key == *key {
                    // A crashed-mid-step record (`running`) is re-executed live;
                    // any terminal record is substituted.
                    match outcome.status {
                        StepStatus::Running => StepPlan::Live,
                        StepStatus::Ok | StepStatus::Error => StepPlan::Replay(outcome),
                    }
                } else {
                    StepPlan::Diverged {
                        recorded: recorded_key,
                    }
                }
            }
            Err(e) => {
                // Resilience first: a read failure degrades to a live attempt
                // rather than stalling the flow (at-least-once still holds — a
                // re-record on resume corrects the journal).
                warn!(flow = %self.flow_id, seq, error = %e, "step_at failed; executing live");
                StepPlan::Live
            }
        }
    }

    /// The `(target)#(args_hash)` key identifying a step, matching
    /// `steps.step_key` and the golden fixtures.
    fn step_key(request: &Request) -> StepKey {
        StepKey::new(format!(
            "{}#{}",
            request.target,
            request.args_hash.as_deref().unwrap_or("-")
        ))
    }

    /// Execute one journaled step: an intercepted effect wrapped in the Tier 1
    /// engine (retries/timeout/breaker are attempts of this one step). On
    /// replay the recorded outcome is substituted and `effect` is never called.
    pub async fn execute_step<F>(&mut self, request: &Request, effect: F) -> Outcome
    where
        F: AsyncFnMut(u32) -> keel_core_api::AttemptResult,
    {
        self.seq += 1;
        let seq = self.seq;
        let key = Self::step_key(request);
        match self.plan_step(seq, &key) {
            StepPlan::Replay(outcome) => replay_outcome(&self.flow_id, seq, &outcome),
            StepPlan::Diverged { recorded } => {
                self.on_divergence(seq, &recorded, &key, request, effect)
                    .await
            }
            StepPlan::Live => {
                self.run_live(seq, &key, StepKind::Effect, request, effect)
                    .await
            }
        }
    }

    /// The `flows.on_nondeterminism` response effective for this handle: the
    /// configured one, except a `code_hash` mismatch downgrades `fail`→`warn`
    /// (a deploy that changed the code is expected to diverge; §4.4).
    fn effective_response(&self) -> NondeterminismResponse {
        let configured = self.engine.nondeterminism_response();
        if self.code_hash_fenced && configured == NondeterminismResponse::Fail {
            NondeterminismResponse::Warn
        } else {
            configured
        }
    }

    /// Apply the nondeterminism policy to a `(seq, step_key)` divergence (§4.4).
    async fn on_divergence<F>(
        &mut self,
        seq: u64,
        recorded: &StepKey,
        observed: &StepKey,
        request: &Request,
        effect: F,
    ) -> Outcome
    where
        F: AsyncFnMut(u32) -> keel_core_api::AttemptResult,
    {
        let (mode, preserve) = match self.effective_response() {
            // Halt with a precise diagnostic; the caller fails the flow.
            NondeterminismResponse::Fail => {
                return diverged_outcome(&self.flow_id, seq, recorded, observed);
            }
            // Continue live from the divergence point, journaling a marker.
            NondeterminismResponse::Warn => ("warn", false),
            // Abandon replay for a fresh attempt, preserving the old records.
            NondeterminismResponse::Branch => ("branch", true),
        };
        let div = Divergence {
            seq,
            recorded,
            observed,
            mode,
            preserve,
        };
        self.branch_and_continue(div, request, effect).await
    }

    /// Journal a branch `marker` and re-execute the divergent step live, then
    /// stay live for the rest of the flow.
    async fn branch_and_continue<F>(
        &mut self,
        div: Divergence<'_>,
        request: &Request,
        effect: F,
    ) -> Outcome
    where
        F: AsyncFnMut(u32) -> keel_core_api::AttemptResult,
    {
        self.journal_branch_marker(&div);
        let live_seq = self.seq;
        self.run_live(live_seq, div.observed, StepKind::Effect, request, effect)
            .await
    }

    /// Journal the branch marker, abandon replay, and advance the cursor to the
    /// live-continuation seq (left in `self.seq`). With `preserve` (`branch`),
    /// the marker + continuation go to a high seq lane so the abandoned run's
    /// records survive for audit; otherwise (`warn`) they continue in place.
    /// Shared by the effect and value (time/random) re-execution paths.
    fn journal_branch_marker(&mut self, div: &Divergence<'_>) {
        warn!(
            flow = %self.flow_id, seq = div.seq, mode = div.mode,
            expected = %div.recorded, observed = %div.observed,
            "flow nondeterminism; abandoning replay"
        );
        self.replay_abandoned = true;
        let marker_seq = if div.preserve {
            BRANCH_SEQ_BASE + div.seq
        } else {
            div.seq
        };
        let now = self.now();
        self.record(
            marker_seq,
            &StepKey::new(format!("flow:branch:{}", div.mode)),
            &StepOutcome {
                kind: StepKind::Marker,
                attempt: 0,
                status: StepStatus::Ok,
                payload: encode_payload(&json!({
                    "mode": div.mode,
                    "expected": div.recorded.as_str(),
                    "observed": div.observed.as_str(),
                })),
                error_class: None,
                started_at: now,
                ended_at: Some(now),
            },
        );
        // The divergent step re-executes in the slot after its marker;
        // subsequent steps continue from there (replay is now abandoned).
        self.seq = marker_seq + 1;
    }

    /// Journal (or replay) a virtualized clock read. On replay the recorded
    /// value is substituted so a resumed flow sees the same time; live, `now_ms`
    /// is recorded and returned (spec §4.4).
    ///
    /// # Errors
    /// `KEEL-E031` if this step diverges from the journal under `fail`.
    pub fn journal_time(&mut self, now_ms: i64) -> Result<i64, KeelError> {
        let bytes = encode_payload(&json!(now_ms)).unwrap_or_default();
        let recorded = self.resolve_value_step(TIME_KEY, StepKind::Time, bytes)?;
        Ok(decode_payload(&recorded)
            .and_then(|v| v.as_i64())
            .unwrap_or(now_ms))
    }

    /// Journal (or replay) a virtualized random draw. On replay the recorded
    /// bytes are substituted; live, `bytes` are recorded and returned.
    ///
    /// # Errors
    /// `KEEL-E031` if this step diverges from the journal under `fail`.
    pub fn journal_random(&mut self, bytes: Vec<u8>) -> Result<Vec<u8>, KeelError> {
        self.resolve_value_step(RANDOM_KEY, StepKind::Random, bytes)
    }

    /// The shared replay/live machinery for a pure value step (time/random):
    /// no side effect, so its recorded payload bytes are the whole outcome.
    fn resolve_value_step(
        &mut self,
        key_str: &str,
        kind: StepKind,
        live_bytes: Vec<u8>,
    ) -> Result<Vec<u8>, KeelError> {
        self.seq += 1;
        let seq = self.seq;
        let key = StepKey::new(key_str);
        match self.plan_step(seq, &key) {
            StepPlan::Replay(outcome) => Ok(outcome.payload.unwrap_or_default()),
            StepPlan::Live => {
                self.record_value(seq, &key, kind, &live_bytes);
                Ok(live_bytes)
            }
            StepPlan::Diverged { recorded } => {
                let (mode, preserve) = match self.effective_response() {
                    NondeterminismResponse::Fail => {
                        return Err(diverged_error(&self.flow_id, seq, &recorded, &key));
                    }
                    NondeterminismResponse::Warn => ("warn", false),
                    NondeterminismResponse::Branch => ("branch", true),
                };
                let div = Divergence {
                    seq,
                    recorded: &recorded,
                    observed: &key,
                    mode,
                    preserve,
                };
                self.journal_branch_marker(&div);
                let live_seq = self.seq;
                self.record_value(live_seq, &key, kind, &live_bytes);
                Ok(live_bytes)
            }
        }
    }

    /// Record a pure value step (time/random): instantaneous, terminal `ok`.
    fn record_value(&self, seq: u64, key: &StepKey, kind: StepKind, bytes: &[u8]) {
        let now = self.now();
        self.record(
            seq,
            key,
            &StepOutcome {
                kind,
                attempt: 0,
                status: StepStatus::Ok,
                payload: Some(bytes.to_vec()),
                error_class: None,
                started_at: now,
                ended_at: Some(now),
            },
        );
    }

    /// Journal a `running` step, run the effect through the engine, and record
    /// the terminal outcome *before* returning it (at-least-once honesty).
    async fn run_live<F>(
        &mut self,
        seq: u64,
        key: &StepKey,
        kind: StepKind,
        request: &Request,
        effect: F,
    ) -> Outcome
    where
        F: AsyncFnMut(u32) -> keel_core_api::AttemptResult,
    {
        let started_at = self.now();
        self.record(
            seq,
            key,
            &StepOutcome {
                kind,
                attempt: 0,
                status: StepStatus::Running,
                payload: None,
                error_class: None,
                started_at,
                ended_at: None,
            },
        );

        let outcome = self.engine.execute(request, effect).await;

        let ended_at = self.now();
        let (status, payload, error_class) = if outcome.result == "ok" {
            (
                StepStatus::Ok,
                outcome.payload.as_ref().and_then(encode_payload),
                None,
            )
        } else {
            (
                StepStatus::Error,
                None,
                outcome.error.as_ref().map(|e| e.class),
            )
        };
        self.record(
            seq,
            key,
            &StepOutcome {
                kind,
                attempt: outcome.attempts,
                status,
                payload,
                error_class,
                started_at,
                ended_at: Some(ended_at),
            },
        );
        outcome
    }

    /// Record a step, degrading a journal failure to a `warn!`. A lost record
    /// costs replay dedup, never correctness: the `running` marker (or its
    /// absence) makes resume re-execute the step, so the effect runs
    /// at-least-once regardless.
    fn record(&self, seq: u64, key: &StepKey, outcome: &StepOutcome) {
        if let Err(e) = self.journal.record_step(&self.flow_id, seq, key, outcome) {
            warn!(flow = %self.flow_id, seq, error = %e, "record_step failed; step not journaled");
        }
    }

    /// Move the flow to a terminal status on scope exit. Idempotent-ish: a
    /// second call re-stamps the status.
    pub fn complete(&mut self, status: FlowStatus) {
        if let Err(e) = self.journal.complete_flow(&self.flow_id, status) {
            warn!(flow = %self.flow_id, error = %e, "complete_flow failed");
        }
        self.completed = true;
    }

    /// Mark the flow `completed` (the success scope exit).
    pub fn complete_success(&mut self) {
        self.complete(FlowStatus::Completed);
    }

    /// Mark the flow `failed` (a non-retryable failure exit).
    pub fn complete_failed(&mut self) {
        self.complete(FlowStatus::Failed);
    }
}

impl Drop for FlowHandle {
    fn drop(&mut self) {
        if let Some(heartbeat) = self.heartbeat.take() {
            heartbeat.abort();
        }
        if !self.completed {
            // A handle dropped without complete() models a crash: the flow stays
            // `running` with its lease and is resumed after the lease expires.
            debug!(flow = %self.flow_id, "flow handle dropped uncompleted; left running for recovery");
        }
    }
}

/// Spawn the lease-renewal heartbeat, if a tokio runtime is available (it always
/// is under the front ends and tests; a bare synchronous caller simply gets no
/// heartbeat rather than a panic). Renews every `ttl/2` against the journal's
/// clock, so a paused-clock test drives it deterministically.
fn spawn_heartbeat(
    journal: Arc<dyn Journal>,
    flow: FlowId,
    holder: ProcessId,
    ttl: Duration,
) -> Option<tokio::task::AbortHandle> {
    if tokio::runtime::Handle::try_current().is_err() {
        return None;
    }
    let period = (ttl / 2).max(Duration::from_millis(1));
    let task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(period);
        interval.tick().await; // the immediate first tick is the initial acquire
        loop {
            interval.tick().await;
            if let Err(e) = journal.acquire_lease(&flow, &holder, ttl) {
                warn!(flow = %flow, error = %e, "lease heartbeat renewal failed");
            }
        }
    });
    Some(task.abort_handle())
}

/// A configuration/internal `KEEL-E040`.
fn internal(message: String) -> KeelError {
    KeelError {
        code: ErrorCode::Internal,
        message,
    }
}

/// Bare (schema-untagged) MessagePack of a step payload — the encoding the
/// golden journal fixtures use (`conformance/fixtures/journal/`).
fn encode_payload(value: &Value) -> Option<Vec<u8>> {
    rmp_serde::to_vec_named(value).ok()
}

fn decode_payload(bytes: &[u8]) -> Option<Value> {
    rmp_serde::from_slice(bytes).ok()
}

/// A synthetic trace id for a step outcome the manager mints (replay /
/// divergence), distinct from the engine's `t-NNNNNN` live ids.
fn step_trace(flow: &FlowId, seq: u64) -> String {
    format!("flow-{flow}-s{seq}")
}

/// Reconstruct an [`Outcome`] from a recorded step — the replay substitution.
fn replay_outcome(flow: &FlowId, seq: u64, step: &StepOutcome) -> Outcome {
    let mut outcome = base_outcome(step_trace(flow, seq));
    outcome.attempts = step.attempt;
    match step.status {
        StepStatus::Ok | StepStatus::Running => {
            outcome.result = String::from("ok");
            outcome.payload = step.payload.as_deref().and_then(decode_payload);
        }
        StepStatus::Error => {
            outcome.error = Some(OutcomeError {
                code: ErrorCode::NonRetryableError,
                class: step.error_class.unwrap_or(ErrorClass::Other),
                http_status: None,
                message: String::from("replayed failed step"),
                original: None,
            });
        }
    }
    outcome
}

/// The precise expected-vs-actual diagnostic for a `(seq, step_key)` divergence
/// (spec §4.4), shared by the outcome and error surfaces.
fn divergence_message(flow: &FlowId, seq: u64, recorded: &StepKey, observed: &StepKey) -> String {
    format!("flow {flow} diverged at step {seq}: expected {recorded}, got {observed} (KEEL-E031)")
}

/// The KEEL-E031 outcome for a divergence on an effect step (`execute_step`).
fn diverged_outcome(flow: &FlowId, seq: u64, recorded: &StepKey, observed: &StepKey) -> Outcome {
    let mut outcome = base_outcome(step_trace(flow, seq));
    outcome.error = Some(OutcomeError {
        code: ErrorCode::FlowNondeterminism,
        class: ErrorClass::Other,
        http_status: None,
        message: divergence_message(flow, seq, recorded, observed),
        original: None,
    });
    outcome
}

/// The KEEL-E031 error for a divergence on a value step (`journal_time` /
/// `journal_random`), which return `Result` rather than an `Outcome`.
fn diverged_error(flow: &FlowId, seq: u64, recorded: &StepKey, observed: &StepKey) -> KeelError {
    KeelError {
        code: ErrorCode::FlowNondeterminism,
        message: divergence_message(flow, seq, recorded, observed),
    }
}

/// A fresh error/replay [`Outcome`] shell with the shared envelope defaults.
fn base_outcome(trace_id: String) -> Outcome {
    Outcome {
        v: ENVELOPE_VERSION,
        result: String::from("error"),
        payload: None,
        error: None,
        attempts: 0,
        from_cache: false,
        waits_ms: Vec::new(),
        throttled: false,
        throttle_wait_ms: 0,
        breaker: keel_core_api::BreakerState::Closed,
        trace_id,
    }
}
