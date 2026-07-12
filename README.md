# Keel

*The SQLite of durable execution — resilience and durability as a library that
lives inside your process, not a service your process talks to.*

```
$ uvx keel run app.py
keel ▸ wrapped 14 call sites (httpx ×9, openai ×4, psycopg ×1) with production defaults — `keel init` to customize
```

Zero code changes. Retries, backoff, timeouts, circuit breakers, rate limits,
and (opt-in) crash-resumable flows — declared in one `keel.toml`, applied at
intercepted call boundaries, backed by a single local journal file. No daemon,
no port, no login.

**Status: contracts frozen (`contracts-v1`) · the real core exists.**
Sprint 0 is complete (frozen contracts, in-memory core stub in three
languages, conformance suite), and the first slice of the real Rust kernel —
`crates/keel-core`, an async tokio engine running the cache → rate →
breaker → timeout → retry chain with enforced timeouts and jittered
backoff — passes the same 15-scenario conformance corpus as the stubs.
Not yet built: discovery journal, OTel, the FFI facade, language front
ends, CLI, and Tier 2 durable flows. The guiding documents:

- [docs/architecture-spec.md](docs/architecture-spec.md) — what Keel is (two-tier semantics, core, journal, front ends)
- [docs/dx-spec.md](docs/dx-spec.md) — why anyone will love it (the product's soul; conflicts resolve in its favor)
- [docs/engineering-manifesto.md](docs/engineering-manifesto.md) — how we build it (repo standards, July 2026 SOTA)
- [docs/sprint-plan.md](docs/sprint-plan.md) — the parallel-team work breakdown this repo follows

## Repo layout

```
contracts/            frozen interfaces (policy schema, FFI, journal, adapter packs) — CCR to change
crates/
  keel-core-api/      contract types (includes contracts/core_api.rs verbatim) + shared typed policy model
  keel-core/          THE REAL CORE (Tier 1): tokio engine — cache/rate/breaker/timeout/retry
  keel-core-stub/     in-memory fake core (Rust) — the reference stub semantics
  keel-conformance/   shared harness pieces (typed scenario model, subset matcher)
python/keel-core-stub/  the same stub, pure Python (unblocks Team B/E)
node/keel-core-stub/    the same stub, pure Node (unblocks Team C/E)
conformance/          scenario matrix + runners; green here is the definition of done
  fixtures/journal/   golden journal databases (completed / interrupted / dead flows)
docs/                 the specs
```

## Running the conformance suite

Every implementation interprets the same `conformance/scenarios/*.json`:

```
$ cargo test -p keel-core --test conformance     # REAL core     → 15/15 (paused tokio clock)
$ cargo test -p keel-core-stub                   # Rust stub     → 15/15
$ python3 conformance/runner.py                  # Python stub   → 15/15
$ cd node/keel-core-stub && node --test          # Node stub     → 15/15
$ python3 conformance/check_schema.py            # policies vs. policy.schema.json
$ python3 conformance/fixtures/journal/build_fixtures.py   # golden journal DBs
```

## How work proceeds

Teams build against the stub and the frozen contracts (see
[contracts/README.md](contracts/README.md) for the change process). The stub
is swapped for the real Rust core on integration day; conformance is the
referee at every step. Team A (real core) has its first slice landed —
remaining slices are the SQLite discovery journal, OTel span emission, the
FFI facade with the PyO3/napi async bridge, and the ≤10µs overhead benchmark
as a CI artifact. Then, per the sprint plan: B (Python front end), C (Node
front end), D (CLI + auto-walk), E (LLM/agent packs), F (demos,
fault-injection proxy, docs). Session context for coding agents lives in
[CLAUDE.md](CLAUDE.md).
