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
//! # The async `execute_step` bridge (front-end binding concern)
//!
//! [`FlowHandle::execute_step`] is `async`, and both the synchronous AND
//! asynchronous intercepted-call paths in the front-end bindings route through
//! it while a flow is open (an earlier v0.1 restriction refused an async
//! intercepted call with KEEL-E005 rather than let it silently downgrade to
//! Tier 1 on the bare [`Engine`]; that refusal is retired now that the bridge is
//! real — see `contracts/error-codes.json`'s retired async-in-flow case and
//! `docs/ccr/0002-async-steps-and-idempotency-injection.md`).
//!
//! `&mut self` is what makes a single call to `execute_step` exclusive, but a
//! flow handle is reachable from *multiple* concurrent async tasks in a
//! language runtime (Python's `asyncio.gather`, Node's `Promise.all`) — so each
//! binding wraps its open handle in an **async** mutex (never a blocking one)
//! that a call acquires with the moral equivalent of `.lock().await` before
//! touching the handle, and holds for the *entire* step: `seq` is claimed the
//! instant `execute_step_with_idempotency_key` is entered (its very first
//! statement, before any `.await`), and the guard is not released until the
//! terminal outcome is recorded. This is what makes concurrent awaited effects
//! **serialize in await order** — normative for every binding, spelled out in
//! conformance/README.md's "Async steps inside a flow" section: a step's
//! position in the journal is fixed by the order its call *reaches* the handle,
//! never by completion order, so replay reproduces the exact same `(seq,
//! step_key)` sequence deterministically. See `crates/keel-py/src/lib.rs`'s
//! `execute_async` (the reference implementation) for the binding-side half of
//! this contract; `FlowHandle` itself contributes only the `&mut self` exclusion
//! and the at-entry `seq` claim above — it holds no lock and knows nothing about
//! any particular language runtime.
//!
//! Replay correctness is checked, not assumed (spec §4.4): the `(seq, step_key)`
//! encountered on replay must match the journal. A `seq` recorded under a
//! *different* key is nondeterminism ([`ErrorCode::FlowNondeterminism`],
//! KEEL-E031), handled per the `flows.on_nondeterminism` policy.
//!
//! # Re-entering a completed flow is pure replay
//!
//! A rerun of a flow that already reached `completed` is a *pure replay*: no
//! lease is taken, no flow-level attempt is consumed, no heartbeat runs, and no
//! effect ever fires — every step is substituted from the journal and the
//! function reconstructs its result from the recorded values. This is what makes
//! `keel run` on a finished pipeline instant and side-effect-free, and it is why
//! re-entry must never surface a lease error (a completed flow is not `running`,
//! so a naive `acquire_lease WHERE status='running'` would wrongly report
//! KEEL-E030). A replay-only handle reaching a step with no recorded outcome is
//! itself a divergence (the code grew a step since completion) and is refused
//! with KEEL-E031 rather than run live.
//!
//! # Step-key convention (who names steps)
//!
//! Step keys are *front-end supplied*, never minted by this manager, so the key
//! a live run records and the key a replay observes are produced by the same
//! code path. Effect steps key on the request as `"(target)#(args_hash)"`
//! ([`FlowHandle::step_key`]). Virtualized value steps (time/random) take an
//! explicit caller key following the same `"<callable>#<args_hash>"` shape with
//! the front end's language prefix and `-` for a niladic read — e.g. the Python
//! front end and the golden fixtures both use `py:time.time#-`,
//! `py:time.time_ns#-`, `py:random.random#-`. Keeping the convention in the
//! caller means the fixtures, the live front end, and replay all agree on one
//! key per read.

use core::time::Duration;
use std::sync::{Arc, Mutex};
use std::time::Instant;

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
/// The field carrying an adapter-injected idempotency key inside a `running`
/// step record's schema-tagged payload (contracts/adapter-pack.md
/// "Idempotency-key injection" rule 3: the minted key is journaled with the
/// step, and a resume that re-executes a crashed step injects the SAME key).
const IDEMPOTENCY_KEY_FIELD: &str = "idempotency_key";

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

    /// Flow-level attempt cap (distinct from Tier 1 step attempts): each
    /// resume of a not-yet-completed flow consumes one attempt from the
    /// reserved seq-0 counter; exceeding the cap marks the flow dead. Returns
    /// the new attempt number, or `Err` if the cap was exceeded.
    fn bump_attempt_or_kill(&self, flow_id: &FlowId, entrypoint: &str) -> Result<u32, KeelError> {
        let prior = self
            .journal
            .step_at(flow_id, ATTEMPT_SEQ)
            .map_err(|e| internal(format!("attempt lookup failed: {e}")))?
            .map_or(0, |(_, o)| o.attempt);
        let attempt = prior.saturating_add(1);
        // A second-or-later attempt is a *recovery*: the flow did not run to
        // completion in one process lifetime (crash, lease loss, deliberate
        // resume). First entries (attempt == 1) are ordinary starts, not
        // recoveries — only re-entries count toward the §4.5 metric.
        if attempt >= 2 {
            crate::metrics::record_flow_resume(entrypoint);
        }
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
        Ok(attempt)
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

        // Re-entering a completed flow is PURE REPLAY (see module docs): hand
        // back a replay-only handle that substitutes every step, takes no lease,
        // spawns no heartbeat, and consumes no attempt. A naive lease acquire
        // here would fail (a completed flow is not `running`) and wrongly report
        // KEEL-E030; instead the rerun reconstructs its result from the journal.
        if status == FlowStatus::Completed {
            return Ok(self.new_handle(
                flow_id.clone(),
                status,
                false,
                None,
                LeaseHeartbeatMonitor::new(),
                true,
            ));
        }

        let attempt = self.bump_attempt_or_kill(flow_id, entrypoint)?;

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

        // Replay is fenced when the recorded code differs from what is running
        // now: a divergence under a changed deploy downgrades fail→warn (§4.4).
        let code_hash_fenced = match (existing.and_then(|f| f.code_hash), current_code_hash) {
            (Some(recorded), Some(current)) => recorded != current,
            _ => false,
        };

        // Seeded now, right after we confirmed via `acquire_lease` that we hold
        // it: the monotonic reference point every subsequent `lease_lost` check
        // measures elapsed real time against (architecture-spec §6).
        let lease_monitor = LeaseHeartbeatMonitor::new();
        let heartbeat = spawn_heartbeat(
            Arc::clone(&self.journal),
            flow_id.clone(),
            self.holder.clone(),
            self.config.lease_ttl,
            lease_monitor.clone(),
        );

        Ok(self.new_handle(
            flow_id.clone(),
            status,
            code_hash_fenced,
            heartbeat,
            lease_monitor,
            false,
        ))
    }

    /// Assemble a [`FlowHandle`] over this manager's shared engine/journal/clock.
    /// `replay_only` marks a completed-flow re-entry (pure replay: every step is
    /// substituted, no effect fires) — the handle is born already `completed` so
    /// dropping it is quiet and a front-end `exit_flow` is a harmless re-stamp.
    /// `lease_monitor` is unused by a replay-only handle (it never runs a live
    /// step, so `lease_lost` is never consulted), but every path constructs one
    /// so the field is never `Option`.
    fn new_handle(
        &self,
        flow_id: FlowId,
        entry_status: FlowStatus,
        code_hash_fenced: bool,
        heartbeat: Option<HeartbeatHandle>,
        lease_monitor: LeaseHeartbeatMonitor,
        replay_only: bool,
    ) -> FlowHandle {
        FlowHandle {
            engine: Arc::clone(&self.engine),
            journal: Arc::clone(&self.journal),
            clock: Arc::clone(&self.clock),
            holder: self.holder.clone(),
            flow_id,
            seq: 0,
            code_hash_fenced,
            replay_abandoned: false,
            replay_only,
            completed: replay_only,
            entry_status,
            heartbeat,
            lease_ttl: self.config.lease_ttl,
            lease_monitor,
        }
    }
}

/// A single running flow. Owns the step cursor (`seq`), so it is used by one
/// task via `&mut`; the manager stays `&self`-concurrent for many flows.
///
/// Dropping a handle without [`complete`](Self::complete) leaves the flow
/// `running` with its lease — exactly the crash shape recovery resumes.
#[expect(
    clippy::struct_excessive_bools,
    reason = "each flag is an independent per-handle replay/lease predicate, not a \
              packed state enum: code_hash_fenced, replay_abandoned, replay_only, completed"
)]
pub struct FlowHandle {
    engine: Arc<Engine>,
    journal: Arc<dyn Journal>,
    clock: Arc<dyn Clock>,
    /// This handle's lease holder id — checked before every live step so a
    /// handle that has lost its lease fences (KEEL-E030) instead of running the
    /// effect a second executor may already be running (double-fire defense).
    holder: ProcessId,
    flow_id: FlowId,
    seq: u64,
    code_hash_fenced: bool,
    replay_abandoned: bool,
    /// A completed-flow re-entry (pure replay): every step is substituted from
    /// the journal and no effect fires; a step with no record is a divergence.
    replay_only: bool,
    /// The flow's status when this handle was opened (`completed` for a pure
    /// replay handle, `running`/`failed` for a live entry) — the front end reads
    /// it to distinguish "resumed" from "already finished".
    entry_status: FlowStatus,
    completed: bool,
    /// The lease-renewal thread; stopped and joined when the handle drops so a
    /// completed or crashed flow stops heart-beating.
    heartbeat: Option<HeartbeatHandle>,
    /// This handle's lease TTL, for [`FlowHandle::lease_lost`]'s monotonic
    /// freshness check.
    lease_ttl: Duration,
    /// Monotonic view of our own last successful lease renewal, updated by the
    /// heartbeat thread — see [`LeaseHeartbeatMonitor`].
    lease_monitor: LeaseHeartbeatMonitor,
}

impl core::fmt::Debug for FlowHandle {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FlowHandle")
            .field("flow_id", &self.flow_id)
            .field("seq", &self.seq)
            .field("entry_status", &self.entry_status)
            .field("replay_only", &self.replay_only)
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

    /// The flow's status when this handle was opened. `completed` means this is a
    /// pure-replay handle (a rerun of an already-finished flow); `running` or
    /// `failed` means a live entry/resume. The front end reads this to tell the
    /// user "resumed" vs. "already completed".
    #[must_use]
    pub fn entry_status(&self) -> FlowStatus {
        self.entry_status
    }

    /// Whether this handle is a completed-flow pure replay — every step is
    /// substituted from the journal and no effect will fire.
    #[must_use]
    pub fn is_replay_only(&self) -> bool {
        self.replay_only
    }

    fn now(&self) -> i64 {
        self.clock.now_ms()
    }

    /// Whether this handle has definitively lost its lease. Two independent
    /// checks (architecture-spec §6: generous TTLs absorb cross-machine clock
    /// skew; heartbeats judge freshness on a *monotonic* clock):
    ///
    /// - **Cross-process arbitration** (journal wall clock, read-only here): has
    ///   another holder's `acquire_lease` overwritten `lease_holder`? That CAS
    ///   already did the wall-clock expiry comparison against the journal's own
    ///   record when it stole the row, so re-deriving it from *our* wall clock
    ///   here would be redundant — and exactly the redundant read that lets a
    ///   local NTP step spuriously flip the verdict.
    /// - **Local freshness** (monotonic, never wall clock): has our own
    ///   heartbeat actually renewed within `lease_ttl` of *real elapsed time*?
    ///   [`LeaseHeartbeatMonitor`] tracks this with [`Instant`], which cannot
    ///   jump backwards or forwards under an NTP correction the way comparing
    ///   two `SystemTime` readings can — so a wall-clock step on this process
    ///   can neither spuriously expire a healthy lease nor paper over a
    ///   genuinely starved heartbeat.
    ///
    /// A missing flow row or a journal read error is *not* treated as loss
    /// (resilience-first: at-least-once still holds via the `running` marker),
    /// so a transient hiccup does not stall a legitimately-held flow.
    fn lease_lost(&self) -> bool {
        let still_recorded_holder = match self.journal.get_flow(&self.flow_id) {
            Ok(Some(flow)) => flow.lease_holder.as_ref() == Some(&self.holder),
            Ok(None) | Err(_) => true,
        };
        !still_recorded_holder || self.lease_monitor.elapsed() >= self.lease_ttl
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
        self.execute_step_with_idempotency_key(request, None, effect)
            .await
    }

    /// [`execute_step`](Self::execute_step), carrying the idempotency key the
    /// adapter minted and injected for this call (contracts/adapter-pack.md
    /// "Idempotency-key injection", rule 3). The key is journaled in the step's
    /// `running` record, so a resume that re-executes a crashed step can read it
    /// back ([`recorded_idempotency_key`](Self::recorded_idempotency_key)) and
    /// inject the SAME key — making the at-least-once re-execution deduplicable
    /// on the provider side. The key never feeds the step key (`args_hash` is
    /// unchanged by injection), so replay matching is unaffected.
    pub async fn execute_step_with_idempotency_key<F>(
        &mut self,
        request: &Request,
        idempotency_key: Option<&str>,
        effect: F,
    ) -> Outcome
    where
        F: AsyncFnMut(u32) -> keel_core_api::AttemptResult,
    {
        self.seq += 1;
        let seq = self.seq;
        let key = Self::step_key(request);
        let plan = self.plan_step(seq, &key);
        // A completed-flow replay never runs an effect: substitute a recorded
        // step, and treat a missing/diverging record as nondeterminism (the code
        // grew or changed a step since the flow completed) rather than firing it.
        if self.replay_only {
            return match plan {
                StepPlan::Replay(outcome) => replay_outcome(&self.flow_id, seq, &outcome),
                _ => replay_miss_outcome(&self.flow_id, seq, &key),
            };
        }
        match plan {
            StepPlan::Replay(outcome) => replay_outcome(&self.flow_id, seq, &outcome),
            StepPlan::Diverged { recorded } => {
                self.on_divergence(seq, &recorded, &key, request, idempotency_key, effect)
                    .await
            }
            StepPlan::Live => {
                self.run_live(
                    seq,
                    &key,
                    StepKind::Effect,
                    request,
                    idempotency_key,
                    effect,
                )
                .await
            }
        }
    }

    /// The idempotency key recorded for the flow's NEXT step, when that record
    /// is a crashed (`running`) step under the same `step_key` — the
    /// resume-reuse read of contracts/adapter-pack.md "Idempotency-key
    /// injection" rule 3. Adapters call this *before* executing an injectable
    /// step: `Some(key)` means a previous run crashed mid-step after minting
    /// `key`, and re-executing with the SAME key lets the provider deduplicate
    /// the at-least-once re-execution. `None` — mint fresh — on a fresh step, a
    /// terminal record (the step will be substituted, no key is sent), a
    /// diverging key, an abandoned replay, a pure-replay handle, or a journal
    /// read failure (resilience first).
    #[must_use]
    pub fn recorded_idempotency_key(&self, step_key: &str) -> Option<String> {
        if self.replay_abandoned || self.replay_only {
            return None;
        }
        let key = StepKey::new(step_key);
        match self.journal.step_at(&self.flow_id, self.seq + 1) {
            Ok(Some((recorded, outcome)))
                if recorded == key && outcome.status == StepStatus::Running =>
            {
                outcome
                    .payload
                    .as_deref()
                    .and_then(decode_payload)
                    .and_then(|v| {
                        v.get(IDEMPOTENCY_KEY_FIELD)
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    })
            }
            _ => None,
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
        idempotency_key: Option<&str>,
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
        self.branch_and_continue(div, request, idempotency_key, effect)
            .await
    }

    /// Journal a branch `marker` and re-execute the divergent step live, then
    /// stay live for the rest of the flow.
    async fn branch_and_continue<F>(
        &mut self,
        div: Divergence<'_>,
        request: &Request,
        idempotency_key: Option<&str>,
        effect: F,
    ) -> Outcome
    where
        F: AsyncFnMut(u32) -> keel_core_api::AttemptResult,
    {
        self.journal_branch_marker(&div);
        let live_seq = self.seq;
        self.run_live(
            live_seq,
            div.observed,
            StepKind::Effect,
            request,
            idempotency_key,
            effect,
        )
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

    /// Journal (or replay) a virtualized clock read under the front-end-supplied
    /// `key` (the module-docs convention, e.g. `py:time.time#-`). On replay the
    /// recorded value is substituted so a resumed flow sees the same time; live,
    /// `now_ms` is recorded and returned (spec §4.4).
    ///
    /// # Errors
    /// `KEEL-E031` if this step diverges from the journal under `fail`.
    pub fn journal_time(&mut self, key: &str, now_ms: i64) -> Result<i64, KeelError> {
        let bytes = encode_payload(&json!(now_ms)).unwrap_or_default();
        let recorded = self.resolve_value_step(key, StepKind::Time, bytes)?;
        Ok(decode_payload(&recorded)
            .and_then(|v| v.as_i64())
            .unwrap_or(now_ms))
    }

    /// Journal (or replay) a virtualized random draw under the front-end-supplied
    /// `key` (e.g. `py:random.random#-`). On replay the recorded bytes are
    /// substituted; live, `bytes` are recorded and returned.
    ///
    /// # Errors
    /// `KEEL-E031` if this step diverges from the journal under `fail`.
    pub fn journal_random(&mut self, key: &str, bytes: Vec<u8>) -> Result<Vec<u8>, KeelError> {
        self.resolve_value_step(key, StepKind::Random, bytes)
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
        let plan = self.plan_step(seq, &key);
        // Pure replay: substitute the recorded value; never re-draw/re-read.
        if self.replay_only {
            return match plan {
                StepPlan::Replay(outcome) => Ok(outcome.payload.unwrap_or_default()),
                _ => Err(replay_miss_error(&self.flow_id, seq, &key)),
            };
        }
        match plan {
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
    /// An adapter-injected `idempotency_key` is journaled IN the `running`
    /// record (payload `{"idempotency_key": …}`), so a resume that re-executes
    /// this step after a crash reads the same key back
    /// ([`recorded_idempotency_key`](Self::recorded_idempotency_key)); the
    /// terminal record replaces the payload with the outcome as before.
    async fn run_live<F>(
        &mut self,
        seq: u64,
        key: &StepKey,
        kind: StepKind,
        request: &Request,
        idempotency_key: Option<&str>,
        effect: F,
    ) -> Outcome
    where
        F: AsyncFnMut(u32) -> keel_core_api::AttemptResult,
    {
        // Lease fence: before firing a live effect, confirm we still hold the
        // lease. If it lapsed (heartbeat starved, clock jump) another process
        // may have stolen it and be resuming this flow concurrently — running
        // the effect now would double-fire it (charges, emails, LLM spend) and
        // interleave step records. Fail the step loudly (KEEL-E030) instead of
        // silently double-executing. A journal read error is resilience-first
        // (proceed): only a definitive loss fences.
        if self.lease_lost() {
            warn!(flow = %self.flow_id, seq, "lease lost before live step; refusing to double-execute (KEEL-E030)");
            return lease_lost_outcome(&self.flow_id, seq);
        }
        let started_at = self.now();
        self.record(
            seq,
            key,
            &StepOutcome {
                kind,
                attempt: 0,
                status: StepStatus::Running,
                payload: idempotency_key
                    .and_then(|k| encode_payload(&json!({ (IDEMPOTENCY_KEY_FIELD): k }))),
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
    /// second call re-stamps the status — except a `completed` flow is immutable
    /// at the journal (`complete_flow` refuses to demote it), so re-running a
    /// finished flow can never flip it to `failed`/`dead`.
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
        // Stop and join the lease-renewal thread (HeartbeatHandle::drop).
        self.heartbeat = None;
        if !self.completed {
            // A handle dropped without complete() models a crash: the flow stays
            // `running` with its lease and is resumed after the lease expires.
            debug!(flow = %self.flow_id, "flow handle dropped uncompleted; left running for recovery");
        }
    }
}

/// The monotonic timestamp of this handle's last successful lease renewal,
/// shared between the heartbeat thread (writer, [`Self::mark_renewed`]) and the
/// owning [`FlowHandle`] (reader, [`Self::elapsed`]). Built on [`Instant`], the
/// standard library's guaranteed-non-decreasing clock — unlike [`SystemTime`]
/// (used for every *stored* journal timestamp), it cannot be stepped backwards
/// by an NTP correction or a manual clock change, which is exactly the
/// property architecture-spec §6 asks lease heartbeats to have.
///
/// [`SystemTime`]: std::time::SystemTime
#[derive(Debug, Clone)]
struct LeaseHeartbeatMonitor(Arc<Mutex<Instant>>);

impl LeaseHeartbeatMonitor {
    /// Starts "fresh" as of now — used both when a lease is first acquired
    /// (before the heartbeat thread has ticked even once) and for the unused
    /// monitor a replay-only handle is handed.
    fn new() -> Self {
        Self(Arc::new(Mutex::new(Instant::now())))
    }

    /// Record a successful renewal at the current instant.
    fn mark_renewed(&self) {
        let mut at = self
            .0
            .lock()
            .expect("lease heartbeat monitor lock poisoned");
        *at = Instant::now();
    }

    /// Real time elapsed since the last successful renewal (or since
    /// construction, if none has happened yet).
    fn elapsed(&self) -> Duration {
        self.0
            .lock()
            .expect("lease heartbeat monitor lock poisoned")
            .elapsed()
    }
}

/// A running lease-renewal thread. Dropping this signals the thread to stop (by
/// disconnecting the channel) and joins it.
struct HeartbeatHandle {
    stop: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for HeartbeatHandle {
    fn drop(&mut self) {
        // Dropping the sender disconnects the channel, waking the thread's
        // `recv_timeout` immediately so it exits without waiting out the period.
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Spawn the lease-renewal heartbeat on a dedicated **OS thread** (not a tokio
/// task). Under the synchronous front-end bindings the ambient runtime is a
/// current-thread runtime that only makes progress while a `block_on` is in
/// flight, so a tokio-task heartbeat starves exactly when it is needed — during
/// a long blocking step or pure-caller compute between steps — letting the lease
/// lapse while the flow is still alive. A std thread renews on real wall-clock
/// time regardless of whether any runtime is being polled. Renews every `ttl/2`
/// against the journal (which does its own wall-clock CAS — see
/// [`FlowHandle::lease_lost`]); every successful renewal also marks `monitor`,
/// the *monotonic* freshness signal the owning handle consults. A lost lease
/// (`Ok(false)`) is logged loudly (the step-level fence is what actually
/// prevents double execution).
fn spawn_heartbeat(
    journal: Arc<dyn Journal>,
    flow: FlowId,
    holder: ProcessId,
    ttl: Duration,
    monitor: LeaseHeartbeatMonitor,
) -> Option<HeartbeatHandle> {
    use std::sync::mpsc::{RecvTimeoutError, channel};

    let period = (ttl / 2).max(Duration::from_millis(1));
    let (stop, rx) = channel::<()>();
    let thread = std::thread::Builder::new()
        .name(String::from("keel-lease-heartbeat"))
        .spawn(move || {
            // Loop until the sender is dropped/sent (handle closing) —
            // `recv_timeout` then yields `Ok`/`Disconnected` and the `while let`
            // ends; a `Timeout` is a renewal tick.
            while let Err(RecvTimeoutError::Timeout) = rx.recv_timeout(period) {
                match journal.acquire_lease(&flow, &holder, ttl) {
                    Ok(true) => monitor.mark_renewed(),
                    Ok(false) => {
                        warn!(flow = %flow, "lease heartbeat: lease lost to another holder");
                    }
                    Err(e) => {
                        warn!(flow = %flow, error = %e, "lease heartbeat renewal failed");
                    }
                }
            }
        })
        .ok()?;
    Some(HeartbeatHandle {
        stop: Some(stop),
        thread: Some(thread),
    })
}

/// A configuration/internal `KEEL-E040`.
fn internal(message: String) -> KeelError {
    KeelError {
        code: ErrorCode::Internal,
        message,
    }
}

/// Self-describing schema tag stamped into every step payload blob, honoring
/// journal.sql's "MessagePack, schema-tagged" contract (`steps.payload`) and
/// mirroring the persistent cache's `keel.cache/v1` convention so one journal.db
/// uses one payload-tagging discipline.
const STEP_PAYLOAD_SCHEMA: &str = "keel.step/v1";

/// The schema-tagged step-payload envelope, written by reference (no clone).
#[derive(serde::Serialize)]
struct StepPayloadRef<'a> {
    schema: &'a str,
    payload: &'a Value,
}

/// The owned form read back before its tag is verified.
#[derive(serde::Deserialize)]
struct StepPayloadOwned {
    schema: String,
    payload: Value,
}

/// MessagePack-encode a step payload with its schema tag (journal.sql:
/// `steps.payload` is "MessagePack, schema-tagged").
fn encode_payload(value: &Value) -> Option<Vec<u8>> {
    rmp_serde::to_vec_named(&StepPayloadRef {
        schema: STEP_PAYLOAD_SCHEMA,
        payload: value,
    })
    .ok()
}

/// Decode a step payload. Prefers the schema-tagged envelope; falls back to a
/// bare value so journals written before the tag existed — including the golden
/// fixtures in `conformance/fixtures/journal/` — still replay (the tag is
/// introduced without a breaking on-disk migration, which is exactly what
/// "versioned, self-describing" buys).
fn decode_payload(bytes: &[u8]) -> Option<Value> {
    if let Ok(envelope) = rmp_serde::from_slice::<StepPayloadOwned>(bytes)
        && envelope.schema == STEP_PAYLOAD_SCHEMA
    {
        return Some(envelope.payload);
    }
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

/// The KEEL-E030 outcome for a live step whose lease was lost to another holder
/// — the step is refused rather than double-executed.
fn lease_lost_outcome(flow: &FlowId, seq: u64) -> Outcome {
    let mut outcome = base_outcome(step_trace(flow, seq));
    outcome.error = Some(OutcomeError {
        code: ErrorCode::FlowLeaseHeld,
        class: ErrorClass::Other,
        http_status: None,
        message: format!(
            "flow {flow} lost its lease before step {seq}; another holder may be resuming it \
             (KEEL-E030). Refusing to run the effect to avoid double execution."
        ),
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

/// A completed-flow replay reached step `seq` (`observed`) with no matching
/// recorded outcome — the code grew or reordered a step since the flow finished.
/// Refused as nondeterminism (KEEL-E031) so no effect fires on a replay handle.
fn replay_miss_message(flow: &FlowId, seq: u64, observed: &StepKey) -> String {
    format!(
        "flow {flow} replay reached unrecorded step {seq} ({observed}); \
         the completed flow's code changed (KEEL-E031)"
    )
}

/// The KEEL-E031 outcome for a replay-only miss on an effect step.
fn replay_miss_outcome(flow: &FlowId, seq: u64, observed: &StepKey) -> Outcome {
    let mut outcome = base_outcome(step_trace(flow, seq));
    outcome.error = Some(OutcomeError {
        code: ErrorCode::FlowNondeterminism,
        class: ErrorClass::Other,
        http_status: None,
        message: replay_miss_message(flow, seq, observed),
        original: None,
    });
    outcome
}

/// The KEEL-E031 error for a replay-only miss on a value step.
fn replay_miss_error(flow: &FlowId, seq: u64, observed: &StepKey) -> KeelError {
    KeelError {
        code: ErrorCode::FlowNondeterminism,
        message: replay_miss_message(flow, seq, observed),
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

#[cfg(test)]
mod tests {
    use super::{LeaseHeartbeatMonitor, decode_payload, encode_payload};
    use core::time::Duration;
    use serde_json::json;

    /// `LeaseHeartbeatMonitor` measures real elapsed time (architecture-spec
    /// §6's monotonic heartbeat), not a value anyone can inject — so this is
    /// necessarily a real-clock test with small real sleeps, the same
    /// precedent `the_heartbeat_renews_the_lease_on_a_real_clock` sets in
    /// `crates/keel-core/tests/flows.rs`.
    #[test]
    fn lease_heartbeat_monitor_tracks_real_elapsed_time_since_the_last_renewal() {
        let monitor = LeaseHeartbeatMonitor::new();
        assert!(
            monitor.elapsed() < Duration::from_millis(50),
            "freshly constructed: elapsed should be ~0"
        );

        std::thread::sleep(Duration::from_millis(60));
        assert!(
            monitor.elapsed() >= Duration::from_millis(60),
            "elapsed grows with real time when nothing renews"
        );

        monitor.mark_renewed();
        assert!(
            monitor.elapsed() < Duration::from_millis(50),
            "a renewal resets elapsed back to ~0"
        );
    }

    #[test]
    fn payload_round_trips_through_the_schema_tag() {
        let value = json!({ "rows": 120, "nested": [1, 2, 3], "ok": true });
        let bytes = encode_payload(&value).expect("encodes");
        assert_eq!(decode_payload(&bytes), Some(value));
    }

    #[test]
    fn legacy_bare_messagepack_still_decodes() {
        // Journals written before the tag (and the golden fixtures) stored the
        // bare value; the decoder must still read them (no breaking migration).
        let map = json!({ "rows": 120 });
        let bare_map = rmp_serde::to_vec_named(&map).expect("bare encodes");
        assert_eq!(decode_payload(&bare_map), Some(map));

        // The fixtures' bare uint32 virtualized-time value (0xCE6A518600).
        let num = json!(1_783_727_616u64);
        let bare_num = rmp_serde::to_vec_named(&num).expect("bare encodes");
        assert_eq!(bare_num, vec![0xCE, 0x6A, 0x51, 0x86, 0x00]);
        assert_eq!(decode_payload(&bare_num), Some(num));
    }
}
