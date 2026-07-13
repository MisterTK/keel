# Keel Engineering Manifesto

*The standards this repo is built to, as of July 11, 2026. Concise on purpose:
if a rule doesn't change what someone writes, it doesn't belong here. The
architecture spec says what we build; the DX spec says why anyone will love
it; this says how we build it. Conflicts resolve in that order.*

## Process

1. **Contracts before code.** Everything in `contracts/` is a compatibility
   promise. Nobody edits it unilaterally: file a contract-change request,
   the orchestrator arbitrates, and the `contract-change-approved` PR label
   is what lets CI pass. Code adapts to contracts, never the reverse.
2. **Conformance is the referee.** "Done" means green on
   `conformance/scenarios/` — for the stub, the real core, and every
   language harness, interpreting the *same* corpus. Not "the author says
   it works." New scenarios are welcome without ceremony; they constrain
   implementations without changing interfaces.
3. **Parity is a feature.** The Rust, Python, and Node implementations keep
   bit-identical semantics, specified once in `conformance/README.md`.
   A behavior that exists in one language and not the others is a bug.
4. **Every simplification is documented.** Stubs and early slices may cut
   corners (virtual clock, fixed-window rate limiting, count-mode breaker)
   — but the corner is written down where the code lives and the
   conformance suite only asserts what all implementations share.
5. **Honest reporting.** Error messages carry what/why/what-next and a
   trace reference. The original error always propagates unchanged. We
   never claim exactly-once; we journal, deduplicate, and say so.

## Rust (the core)

6. **Latest stable, pinned.** `rust-toolchain.toml` pins the exact current
   stable (today: 1.97.0, released 2026-07-07); bumps are deliberate
   commits, not drift. Edition 2024, resolver 3, `rust-version` tracks the
   pin. `Cargo.lock` is committed and CI runs `--locked` — reproducible
   builds for libraries and binaries alike.
7. **The type system is the validator.** Parse, don't check: policy
   documents deserialize into typed structs where invalid states are
   unrepresentable — `NonZero*` for counts that can't be zero, newtypes
   whose `FromStr`/`Deserialize` do the literal parsing (`DurationMs`,
   `Rate`, `Schedule`), closed enums for condition sets so unknown values
   fail configuration instead of silently never matching. Every rejection
   carries an exact field path (`serde_path_to_error`).
8. **Structs own behavior.** State machines are types with methods
   (`Breaker::admit`, `RateWindow::plan_admit`), decisions are enums
   (`Admission`), reports are `Serialize` structs — never hand-assembled
   JSON, never `serde_json::Value` plumbing inside domain logic. `Value`
   appears only at boundaries (payloads we round-trip, envelopes at FFI).
9. **The lint wall is load-bearing.** Workspace lints: `unsafe_code =
   "forbid"`, `clippy::all` + `clippy::pedantic`, enforced as errors in CI
   (`-D warnings`). Deviations use `#[expect(lint, reason = "...")]` at the
   narrowest scope — a blanket `allow` without a reason is a review defect.
   The short curated allow-list lives in the root `Cargo.toml` under a
   comment justifying the opt-outs as a group.
10. **Async discipline.** tokio; effects are `AsyncFnMut` closures; locks
    are scoped and *never* held across an `await`; waits are real
    `tokio::time` sleeps so production sleeps wall-clock while tests run
    under `start_paused` virtual time. No test ever sleeps real time,
    except the handful of lease-heartbeat/DB-clock tests that are
    inherently real-clock (dedicated-OS-thread heartbeat measuring wall
    time; Postgres-server-clock arbitration) — each documented in place
    (see `crates/keel-core/tests/flows.rs`'s
    `the_heartbeat_renews_the_lease_on_a_real_clock`,
    `crates/keel-core/src/flow.rs`'s heartbeat-monitor test, and
    `crates/keel-journal/tests/postgres_journal.rs`).
11. **Functions stay small because they're decomposed, not suppressed.**
    When `too_many_lines` fires, extract phase methods (`begin_call`,
    `throttle`, `admit`, `settle`) — don't `allow` it away.

## Python & Node (the front ends)

12. **Python:** 3.11+, full type hints, stdlib-first (the stub and runner
    have zero runtime dependencies). **Node:** ESM (`type: "module"`),
    `node:test` for testing, `.d.ts` alongside untranspiled `.mjs`, zero
    runtime dependencies. In both: the same typed-model mindset as Rust —
    parse and validate at configure time, fail loudly with the frozen
    error codes.

## Testing

13. **Determinism by construction.** Virtual/paused clocks everywhere;
    jitter-free schedules in shared scenarios (jitter is property-tested
    separately with seeded RNG and bounds assertions); reports have sorted
    keys and no wall-clock timestamps. Identical inputs → byte-identical
    output, because agents and humans both diff outputs to detect change.
14. **Harnesses share their interpreter.** Scenario parsing, scripted
    effects, and subset matching live once per language
    (`crates/keel-conformance` in Rust) — two harnesses interpreting the
    corpus differently is how conformance suites rot.
15. **Test names state behavior**, doc comments state the rule being
    verified (`non_idempotent_timeout_keeps_e014`), and a test that can't
    fail for the stated reason doesn't ship.

## Documentation

16. **Docs are contracts too.** `conformance/README.md` is normative;
    module docs state each component's deliberate simplifications; comments
    state constraints the code can't express — never narration of what the
    next line does. When code and docs disagree, whichever is wrong gets
    fixed in the same commit that noticed.
17. **Commit messages explain why and what changed at the semantic level**,
    and every push that changes behavior re-runs the full verification
    matrix (fmt, clippy-pedantic, all Rust suites, Python runner, Node
    tests, schema check) before it leaves the machine.
