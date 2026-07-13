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
   - otherwise wait `min(base·factor^(n−1), cap)` per the schedule
     (default `exp(200ms, x2, max 30s)`), overridden upward by the error's
     `retry_after_ms` (`wait = max(schedule, retry_after)`), and try again.
6. **Outcome.** Terminal errors carry the original error (class,
   http_status, `original` token) so front ends re-raise it unchanged.
   `attempts` counts effect invocations (0 for cache hits and breaker
   rejections). `waits_ms` lists retry backoffs only.
7. **Report.** Deterministic JSON: `{v, clock_ms, targets}` with per-target
   `attempts, breaker_opens, breaker_state, cache_hits, calls, failures,
   retries, successes, throttled` (sorted target keys). `successes` includes
   cache hits; `failures` includes breaker rejections.

## Determinism rules for scenarios

Scenarios use jitter-free schedules so `waits_ms` is exactly assertable by
every implementation. The real core's jitter is validated by its own
property tests, not here. Virtual-clock control (`advance_ms`) maps to
`advance_clock` on the stub and to the test clock in the real core's
harness.

## Adding scenarios

New scenarios are welcome without a CCR (they constrain implementations,
they don't change interfaces) — unless a scenario forces an envelope or
policy change, which goes through the normal contract process.
