# CCR-2 — async steps in flows (retire the KEEL-E005 async case) + idempotency-key injection

- **Status:** submitted 2026-07-12, approved and merged 2026-07-13 via PR #6
  (`76cb94b`), which carried the required `contract-change-approved` label
  per `contracts/README.md`.
- **Precedent:** CCR-1 (PR #5, commit `135580d`), which created
  KEEL-E005 unsupported-configuration and settled the effective-policy
  contract for `keel_configure`. This CCR follows its shape: the contract
  edits land with the CCR; the implementation ripple lands separately.
- **Contract files touched (minimally):** `contracts/error-codes.json`
  (KEEL-E005 `why`/`next` copy), `contracts/adapter-pack.md` (new
  "Idempotency-key injection" section). No ABI, envelope, schema, or grammar
  changes; no error code is added, removed, or renumbered.

## Change 1 — retire the async-in-flow KEEL-E005 case

**What.** KEEL-E005's v0.1 trigger list included "an async effect inside a
durable flow (flows are synchronous-only)". That case is retired: async
intercepted effects inside flows become **supported**, governed by the new
normative ordering rule in `conformance/README.md` ("Async steps inside a
flow"): an open flow handle is a serialization point — concurrent async
effects within one flow are serialized in await order (handle-entry order,
never effect-completion order), and replay requires the same
`(seq, step_key)` sequence, with divergence handled by the existing
`flows.on_nondeterminism` machinery (KEEL-E031).

**Why.** The refusal was a binding limitation, not a semantic one:
`FlowHandle::execute_step` has been async in the core all along
(`crates/keel-core/src/flow.rs`); only the PyO3/napi bridges lacked a path
that routes an async intercepted effect through the open handle while
preserving the single-owner seq cursor. The ordering rule resolves the design
question that blocked the bridge (what deterministic sequence do concurrent
awaits produce?), so the contract case that codified the gap goes away.
KEEL-E005 itself remains frozen and still covers the genuine
capability-missing cases: a durable flow with no journal to record to, and
`KEEL_BACKEND=native` with no native module installed.

**Migration / ripple (landed, PR #6).** The async `execute_step` bridge in
`crates/keel-py/src/lib.rs` (`execute_async`) and
`crates/keel-node/src/lib.rs` (`execute_async`) now routes an open flow's
intercepted effects through `FlowHandle` via an async mutex instead of
refusing with KEEL-E005; the `flow.rs` module doc, front-end README
limitation sections, and `llms-full.txt`'s old "flows are synchronous-only"
line were all updated accordingly. Teams affected: bindings
(keel-py/keel-node), both language front ends, docs. The stubs are
unaffected (they never implemented Tier 2).

## Change 2 — idempotency-key injection semantics (adapter-pack contract)

**What.** `contracts/adapter-pack.md` gains a normative "Idempotency-key
injection" section. Until now the contract only covered *recognition* (a
caller-supplied `Idempotency-Key` makes a POST retryable); the
`idempotency = { header = "..." }` policy knob (already in
`policy.schema.json`, already promised by architecture-spec FR1
"idempotency-key injection to matching call sites") had no specified
injection behavior. The decided semantics:

1. On an unsafe-method call (outside the adapter's idempotent-method set,
   e.g. POST/PATCH) to a target whose effective policy configures
   `idempotency.header`, and where the caller supplied no recognized key of
   their own, the **adapter mints one opaque key per logical call** and
   injects it as the configured header. Caller-supplied keys always win.
2. The key is minted **before the first attempt** and reused verbatim on
   every Tier 1 retry attempt — stable across retries, never per-attempt.
3. Inside a durable flow the key is **journaled with the step record**, and a
   resume that re-executes a crashed (`running`) step injects the **same**
   recorded key — the at-least-once re-execution is thereby deduplicable on
   the provider side (the NFR4/§7 honesty story made real).
4. Injection **flips the adapter's `idempotent` judgment to `true`**, so the
   core's retry layer applies instead of terminating with KEEL-E014.
5. The injected key must **not** feed `args_hash`: it is not part of the
   caller's arguments, and folding it in would change the Tier 2 step key
   `(target)#(args_hash)` across re-executions and fence replay (KEEL-E031).

**Why this shape.** Injection lives in the adapter, not the core: the adapter
is closest to the library (it already owns the `idempotent`/`args_hash`
judgments per the `targets()` operation), front ends already hold the
effective policy (CCR-1), and `core_api.rs` anticipated exactly this —
`Request.idempotent` is documented as "idempotent method, **or an idempotency
key was injected**". Consequently **no envelope or FFI change is needed**;
the existing `Request` carries everything the core must know.

**Migration / ripple (landed, PR #6, Wave 1 / R4).** `idempotency` now
surfaces through `ResolvedPolicy`/`resolve()`, the httpx/requests/fetch
adapters mint+inject keys, `FlowHandle` journals the key on the running step
record and exposes it via `recorded_idempotency_key` for resume-reuse, and
conformance scenario 24 exercises the flipped judgment end to end.
`TargetDecl` (stubs in `contracts/stubs/`) is unchanged — `idempotency_rule`
already describes "how `idempotent` is derived at the seam", which now
includes injection.

## Verification carried with this CCR

- `scripts/sync-vendored.sh` run after the `contracts/` edits; the synced
  vendored copy (`crates/keel-cli/contract/error-codes.json`) committed.
- CLI golden `crates/keel-cli/tests/golden/explain_e005.json` regenerated
  (byte-exact `keel explain KEEL-E005 --json` with the new copy).
- `conformance/check_schema.py`, the conformance runners, and the journal
  fixtures builder stay green — no scenario or schema is touched by the
  contract edits.
