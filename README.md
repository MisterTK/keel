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

**Status: Sprint 0 complete — contracts frozen (`contracts-v1`).** This repo
currently contains the frozen interface contracts, the in-memory core stub in
three languages, and the conformance suite that referees every future
component. The specs are the source of truth:

- [docs/architecture-spec.md](docs/architecture-spec.md) — what Keel is (two-tier semantics, core, journal, front ends)
- [docs/dx-spec.md](docs/dx-spec.md) — why anyone will love it (the product's soul; conflicts resolve in its favor)
- [docs/sprint-plan.md](docs/sprint-plan.md) — the parallel-team work breakdown this repo follows

## Repo layout

```
contracts/            frozen interfaces (policy schema, FFI, journal, adapter packs) — CCR to change
crates/
  keel-core-api/      contract types as a Rust crate (includes contracts/core_api.rs verbatim)
  keel-core-stub/     in-memory fake core (Rust) — the reference stub semantics
python/keel-core-stub/  the same stub, pure Python (unblocks Team B/E)
node/keel-core-stub/    the same stub, pure Node (unblocks Team C/E)
conformance/          scenario matrix + runners; green here is the definition of done
  fixtures/journal/   golden journal databases (completed / interrupted / dead flows)
docs/                 the specs
```

## Running the conformance suite

Every implementation interprets the same `conformance/scenarios/*.json`:

```
$ python3 conformance/runner.py                  # Python stub   → 15/15
$ cargo test -p keel-core-stub                   # Rust stub     → 15/15
$ cd node/keel-core-stub && node --test          # Node stub     → 15/15
$ python3 conformance/check_schema.py            # policies vs. policy.schema.json
$ python3 conformance/fixtures/journal/build_fixtures.py   # golden journal DBs
```

## How work proceeds

Teams build against the stub and the frozen contracts (see
[contracts/README.md](contracts/README.md) for the change process). The stub
is swapped for the real Rust core on integration day; conformance is the
referee at every step. Next up, per the sprint plan: Team A (real core),
B (Python front end), C (Node front end), D (CLI + auto-walk), E (LLM/agent
packs), F (demos, fault-injection proxy, docs).
