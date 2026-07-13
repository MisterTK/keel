# conformance/ — the referee

A shared scenario corpus that every Keel core implementation must pass — the
stubs and the real core alike, in every language harness. "Done" means green
here, not "the author says it works."

## Running

```
$ cargo test -p keel-core --test conformance    # REAL core (paused tokio clock)
$ cargo test -p keel-core-stub                  # Rust stub (virtual clock)
$ python3 conformance/runner.py                 # Python stub
$ (cd node/keel-core-stub && node --test)       # Node stub
```

All four interpret the same `scenarios/*.json`. The Rust harnesses share
their interpreter (`crates/keel-conformance`: typed scenario model, scripted
effects, subset matcher) so they cannot drift on scenario semantics.

Scenarios marked `"tier": 2` (durable flows) need a journal, which only the
real core has: they run in `crates/keel-core/tests/flows_conformance.rs` and
every stub harness skips them. Tier 1 scenarios carry no `tier` field.

## Scenario format

```jsonc
{
  "name": "retry-5xx-then-success",
  "description": "…",
  "policy": { /* keel.toml as JSON, per contracts/policy.schema.json */ },
  "steps": [
    {
      "call": {
        "target": "api.example.com",
        "request": { "op": "GET …", "idempotent": true, "args_hash": "h1" },
        "effect": [ /* AttemptResult per attempt, in order */ ],
        "expect": { /* subset-matched against the Outcome envelope */ }
      }
    },
    { "advance_ms": 15000 },              // advance the core clock
    { "report_expect": { /* subset-matched against report() */ } }
  ]
}
```

- `effect` scripts the underlying call: element N is what attempt N returns
  (an `AttemptResult` envelope from contracts/core_api.rs). Using more
  attempts than scripted is a harness failure.
- `expect` / `report_expect` are **subset matches**: objects require the
  given keys to match (recursively), arrays must match exactly, scalars must
  be equal. Unlisted Outcome fields are unconstrained.

## Execution semantics (normative for every implementation)

Layer order per call: **cache → rate → breaker → retry** (timeout and
journal layers sit inside this order in the real core; scenarios inject
`timeout` as an error class instead of enforcing wall-clock timeouts).

1. **Resolution.** Per-layer: `target."<id>"` entry, else `defaults.llm` when
   the target starts with `llm:`, else `defaults.outbound`. A layer set at a
   more specific level replaces the whole layer table (no deep merge).
   Scenarios use exact target ids (glob/pattern resolution is a front-end
   concern and tested separately).
2. **Cache.** When the resolved policy has `cache` with a `ttl` and the
   request carries `args_hash`: a fresh entry returns `from_cache: true`
   with `attempts: 0` and no effect invocation; a successful live call
   stores its payload for `ttl`.
3. **Rate.** Token bucket, bit-identical across the real core and every stub
   (parity rule): burst capacity equals the rate's `limit`, refilling
   continuously at `limit` scaled units per elapsed millisecond (1 token =
   `window_ms` scaled units — fixed-point integer arithmetic everywhere, no
   float drift). Each admission pre-books one token; exceeding the rate
   delays the call (`throttled: true`, `throttle_wait_ms` = the deficit's
   refill time, rounded up) rather than failing it — the delay is then
   applied to the clock (the real core actually sleeps it; every stub
   advances its virtual clock by it), so the *next* call sees the refilled
   state. Because the arithmetic is pinned exactly, scenarios may assert
   `throttle_wait_ms` (and derived `clock_ms`), not just `throttled` —
   see `20-rate-limit-token-bucket-burst-then-refill.json`; scenarios
   written before token-bucket parity landed (e.g. `13-rate-limit-storm`)
   still deliberately assert only `throttled`.
4. **Breaker.** Observes post-retry call outcomes (never cache hits or
   breaker rejections, which don't invoke the effect). Two modes, selected
   per the frozen schema (`$defs/breaker`: "Setting `failures` selects
   count mode"):
   - **Count mode** — when `failures` is set, or when neither `window` nor
     `failure_rate` is set (`failures` defaults to 5): `failures`
     consecutive terminal failures open the breaker. Rate knobs present
     alongside `failures` are inert (count mode wins).
   - **Rate mode** — when `failures` is absent and both `window` and
     `failure_rate` are set: every post-retry outcome is recorded at its
     completion time on the core clock; after recording a terminal
     failure, the breaker opens iff the trailing `window` holds at least
     `min_calls` outcomes (default 10) and `failed / total >=
     failure_rate` (IEEE double division). An outcome recorded at `t` is
     inside the window while `now - t < window` (strict: an outcome
     exactly `window` old is evicted). Opening — and a half-open probe
     success closing — clears the recorded outcomes.

   A rate-mode knob (`window`, `failure_rate`, `min_calls`) without both
   `window` and `failure_rate` present (and without `failures`) is
   `KEEL-E001` at configure time. While open (both modes), calls fail fast
   with `KEEL-E012` and `attempts: 0`; after `cooldown` (default 15s), one
   probe is admitted (half-open) — success closes, failure re-opens for
   another cooldown.
5. **Retry.** `attempts` is the total attempt budget (default 3 when a
   `retry` table is present; 1 when absent). After a failed attempt, in
   order:
   - error class not matched by `retry.on` (default `["conn", "timeout",
     "429", "5xx"]`) → terminal `KEEL-E015`;
   - attempt budget exhausted → terminal `KEEL-E010`;
   - request not idempotent → terminal `KEEL-E014` (observed, not retried —
     Level 0 hard rule);
   - otherwise wait per the schedule (default `exp(200ms, x2, max 30s)`; a
     single `exp` waits `min(base·factor^(n−1), cap)`, composition per
     "Schedule algebra" below), overridden upward by the error's
     `retry_after_ms` (`wait = max(schedule, retry_after)`), and try again.
6. **Outcome.** Terminal errors carry the original error (class,
   http_status, `original` token) so front ends re-raise it unchanged.
   `attempts` counts effect invocations (0 for cache hits and breaker
   rejections). `waits_ms` lists retry backoffs only.
7. **Report.** Deterministic JSON: `{v, clock_ms, targets}` with per-target
   `attempts, breaker_opens, breaker_state, cache_hits, calls, failures,
   retries, successes, throttled` (sorted target keys). `successes` includes
   cache hits; `failures` includes breaker rejections.

## Schedule algebra: `upTo` / `andThen` (normative)

The full frozen grammar (contracts/schedule-grammar.ebnf) is implemented: a
schedule is one or more `andThen`-separated **segments**, each a primary
(`exp(…)` or `fixed(…)`) with an optional cumulative-wait bound
(`upTo <duration>`). Scenarios 21–22 pin these semantics.

1. **Shape rule (configure time, KEEL-E001).** `upTo` must appear on every
   segment except the last, and never on the last. The grammar defines a
   schedule as a total mapping attempt → wait: a bounded final segment could
   not supply waits past its bound, and an unbounded non-final segment would
   never hand off (unreachable dead config) — both shapes are grammatical but
   invalid, and are rejected loudly at configure time. A single-segment
   schedule (every v0.1 form) therefore never carries `upTo` and behaves
   exactly as before.
2. **Wait computation.** `wait(n)` is a pure function of the retry attempt
   number `n` (1-based; the wait happens after attempt `n` fails). Walk the
   segments left to right with per-segment state — local attempt `a`
   (starting at 1) and emitted total `e` (starting at 0 ms):
   - candidate `w` = the active segment's natural wait at `a`
     (`exp`: `min(base·factor^(a−1), cap)`; `fixed`: the period);
   - while the active segment is bounded and `e + w` exceeds its bound, hand
     off: advance to the next segment (reset `a = 1`, `e = 0`) and recompute
     `w` — a segment whose bound is smaller than its first natural wait
     contributes zero waits, and the handoff cascades;
   - otherwise emit `w`, then `a += 1`, `e += w`.
   An exact fit (`e + w` equals the bound) stays in the segment. Each
   segment's primary restarts at local attempt 1 on entry (an `exp` after
   `andThen` restarts from its base).
3. **What the bound measures.** `upTo` bounds the segment's own cumulative
   **natural** wait: the pre-jitter, pre-`Retry-After` integer waits the walk
   emits. Jitter (real core only) applies to an emitted wait when the segment
   that emitted it says `jitter`; a server override
   (`wait = max(schedule, retry_after)`) applies after the walk. Neither
   feeds back into the walk, so the segment handoff points are identical in
   every implementation and on replay.

Example: `exp(1s, x2) upTo 4s andThen fixed(500ms)` waits 1000, 2000
(cumulative 3000; the natural 4000 would overshoot the 4s bound), then 500,
500, … And in `fixed(1s) upTo 3s andThen fixed(10s) upTo 5s andThen
fixed(250ms)`: three 1000 waits fill the 3s bound exactly, `fixed(10s) upTo
5s` contributes nothing (10000 > 5000 — cascade), and the tail emits 250 ms
waits from the fourth retry on.

## Determinism rules for scenarios

Scenarios use jitter-free schedules so `waits_ms` is exactly assertable by
every implementation. The real core's jitter is validated by its own
property tests, not here. Virtual-clock control (`advance_ms`) maps to
`advance_clock` on the stub and to the test clock in the real core's
harness.

## Tier 2 scenario format (durable flows)

```jsonc
{
  "name": "flow-resume-substitutes-steps",
  "tier": 2,                            // marks a flow scenario; stubs skip it
  "policy": { "flows": { "on_nondeterminism": "fail" } },
  "steps": [],                          // the Tier 1 step list stays empty
  "flow": {
    "entrypoint": "py:pipeline.ingest:main",
    "args_hash": "ah-16",
    "code_hash": "ch-16"                // optional; fences replay across deploys
  },
  "runs": [                             // executions of the SAME flow identity
    {
      "end": "crash",                   // "success" | "failed" | "crash"
      "expect_effect_calls": 2,         // live effect invocations in this run
      "steps": [
        {
          "target": "api.source.internal",
          "args_hash": "q1",
          "effect": { /* AttemptResult for this step's single attempt */ },
          "expect": { /* subset-matched against the step's Outcome */ }
        }
      ]
    }
  ]
}
```

- Each `runs[i]` enters the flow with the same identity; `end` is how the run
  leaves: `success`/`failed` complete the flow, `crash` drops the handle
  mid-flight and advances the clock past the lease TTL (the recovery shape).
- `expect_effect_calls` counts how many step effects actually ran live in the
  run — a replayed (substituted) step must not invoke its effect.

## Tier 2 execution semantics (normative for every implementation)

Reference implementation: `crates/keel-core/src/flow.rs`, exercised by
scenarios 16–17. Golden journal fixtures: `conformance/fixtures/journal/`
(built by `build_fixtures.py`; on-disk schema per `contracts/journal.sql`).

1. **Identity and entry.** A flow's identity is `(entrypoint, args_hash,
   explicit_key?)`; its storage id is the deterministic
   `"(entrypoint)#(args_hash)#(explicit_key or empty)"` — no clock or random
   draw, so a rerun with the same identity opens the same flow row (opening
   is idempotent). `code_hash` is recorded at first entry and compared on
   resume.
2. **Step numbering and keys.** Real steps are numbered `seq = 1, 2, …` in
   execution order. Seq 0 is reserved for the flow-level attempt counter (a
   `marker` step under the key `flow:attempt`). An effect step's key is
   `"(target)#(args_hash)"`, with `-` for a missing args_hash. Value steps
   (virtualized time/random) use a front-end-supplied key of the same shape
   with a language prefix and `-` for a niladic read — e.g. `py:time.time#-`,
   `py:random.random#-`. Step keys are minted by the front end, never by the
   core, so live runs and replays derive keys from the same code path.
3. **Step resolution (replay substitution).** For the step at `(seq, key)`:
   - no journal record → run **live**;
   - a record under the **same key** with a terminal status → **substitute**:
     the effect is never invoked; an `ok` record replays `result: "ok"` with
     the recorded payload and `attempts` = the recorded attempt count; an
     `error` record replays a terminal error carrying `KEEL-E015` and the
     recorded error class;
   - a record under the same key still marked `running` (a crash mid-step) →
     re-execute **live** (the at-least-once shape);
   - a record under a **different key** → **divergence** (rule 6).
4. **At-least-once honesty.** A live step is journaled `running` *before* its
   effect fires and its terminal outcome is recorded *before* the result is
   released to the caller. A journal **write** failure degrades to a warning
   (a lost record costs replay dedup, never correctness: the `running`
   marker, or its absence, makes resume re-execute the step); a journal
   **read** failure during resolution degrades to a live attempt.
5. **Tier boundary.** Retries *within* a step are the Tier 1 engine's
   business: the step's effect runs through the full Tier 1 chain and its
   attempts are journaled as that one step's `attempt` count. Re-execution
   *of the flow* is Tier 2's business. The two never contaminate each other.
6. **Nondeterminism defense.** The effective response is
   `flows.on_nondeterminism` (`fail` default), except a `code_hash` mismatch
   between the recorded flow and the current deploy downgrades `fail` → `warn`.
   - `fail`: the step resolves to a `KEEL-E031` error naming the flow, the
     seq, and expected vs. observed step keys; the divergent effect is never
     invoked.
   - `warn`: journal a `flow:branch:warn` marker at the divergent seq,
     abandon replay (every subsequent step runs live), and re-execute the
     divergent step live at the next seq.
   - `branch`: as `warn`, but the marker (`flow:branch:branch`) and the live
     continuation are written in a high seq lane (base 1 000 000 + seq) so
     the abandoned run's records (seqs `1..`) are preserved for audit.
7. **Leases.** Entering a live (not completed) flow acquires a TTL lease
   (default 30 s) renewed at TTL/2 by a heartbeat; entry while another
   holder's lease is valid fails with `KEEL-E030`. Before each live step the
   handle re-checks its lease: a definitively lost lease refuses the step
   with `KEEL-E030` rather than risking double execution (this per-step fence
   is the actual double-fire defense; the heartbeat only keeps the lease
   fresh). Only definitive loss fences — a journal read error does not.
8. **Attempt cap.** Every live entry/resume of a not-yet-completed flow
   consumes one flow-level attempt (recorded at seq 0). Exceeding
   `max_attempts` (default 3) marks the flow `dead` and fails entry with
   `KEEL-E032`; a dead flow is never auto-resumed. Flow attempts are distinct
   from Tier 1 step attempts.
9. **Crash and recovery model.** A handle dropped without completing leaves
   the flow `running` with its lease — the crash shape. Recovery scans for
   incomplete flows with expired leases and re-executes them from the top,
   substituting per rule 3. A cleanly `failed` flow is reset to `running`
   before a resume re-leases it.
10. **Completed flows are pure replay.** Re-entering a `completed` flow takes
    no lease, runs no heartbeat, and consumes no attempt; every step is
    substituted and no effect ever fires. Reaching a step with no matching
    record on a pure-replay handle (the code changed since completion) is
    refused with `KEEL-E031` rather than run live. A completed flow is
    immutable: re-completion never demotes it to `failed`/`dead`.
11. **Time/random virtualization.** Inside a flow, virtualized clock reads
    and random draws are journaled as value steps (`kind` = `time`/`random`,
    instantaneous, terminal `ok`, `attempt` 0, payload = the value); replay
    substitutes the recorded value so a resumed flow observes the same time
    and randomness. Divergence on a value step follows rule 6.

### Async steps inside a flow (ordering rule)

Normative for the async `execute_step` bridge in the language bindings
(async intercepted effects inside flows; supersedes the retired v0.1
KEEL-E005 async-in-flow refusal):

- **The open flow handle is a serialization point: it admits one step at a
  time.** An async intercepted effect inside a flow claims the next `seq`
  when its call *enters the handle* and holds the admission until its
  terminal outcome is recorded (or its substitution is resolved). A second
  concurrent effect in the same flow — `asyncio.gather`, `Promise.all` —
  waits asynchronously (without blocking the runtime) until the current step
  finishes, then claims the next seq.
- **Concurrent async effects within one flow are therefore SERIALIZED in
  await order** — the order in which their intercepted calls reach the flow
  handle under the language runtime's scheduler. A step's position in the
  journal is fixed by handle-entry order, never by effect completion order.
  Within-flow parallelism is traded for a deterministic, replayable
  `(seq, step_key)` sequence; outside flows, Tier 1 async concurrency is
  unchanged.
- **Replay requires the same `(seq, step_key)` sequence.** If user code lets
  the runtime reach the handle in a different order on resume (racing tasks
  whose scheduling differs run-to-run), that is nondeterminism — detected
  and handled per `flows.on_nondeterminism` (rule 6, `KEEL-E031` under
  `fail`). Keep dispatch order deterministic inside flows: await effects
  sequentially, or fan out in a fixed, data-independent order.
- Value steps (`journal_time`/`journal_random`) participate in the same
  serialized sequence under the same rule.

## Adding scenarios

New scenarios are welcome without a CCR (they constrain implementations,
they don't change interfaces) — unless a scenario forces an envelope or
policy change, which goes through the normal contract process.
