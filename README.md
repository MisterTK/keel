# Keel

*The SQLite of durable execution — resilience and durability as a library that
lives inside your process, not a service your process talks to.*

```
$ keel run app.py
keel ▸ wrapped 14 call sites (httpx ×9, openai ×4, psycopg ×1) with production defaults — `keel init` to customize
```

Zero code changes. Retries, backoff, timeouts, circuit breakers, rate limits, an
LLM dev-cache, and (opt-in) crash-resumable flows — declared in one `keel.toml`,
applied at intercepted call boundaries, backed by a single local journal file.
No daemon, no port, no login. Uninstalling the package removes the behavior and
nothing else; your code runs identically without it.

## Status (2026-07-13)

Tier 1 **and** Tier 2 (including async durable flows) work end-to-end in
**both languages** on the **real native core**, plus the CLI, a Postgres
journal backend for fleet durability, and a scoped Rust front end:

- **Real Rust core** (`crates/keel-core`): async tokio engine, layer chain
  cache → rate (token-bucket) → breaker (count or failure-rate mode) →
  timeout → retry (with schedule composition), per-attempt timeouts,
  equal-jitter backoff, idempotency-key injection, a Tier-1 NDJSON event feed
  (`keel tail`), SQLite or Postgres discovery + flow journal, opt-in OTel
  spans and metrics (off by default — see below), and an FFI facade
  (`crates/keel-ffi`, MessagePack over a C ABI) with PyO3 + napi async
  bridges, both including the Tier-2 flow surface.
- **Python front end** (`python/keel`): `keel run`, import-hook wrapping,
  httpx/requests/aiohttp/urllib3/boto3/psycopg adapters, `llm:` packs
  (openai/anthropic/google-genai) with the dev cache and model fallback
  chains, `tool:`/`mcp:` semantic targets, agent-framework packs
  (pydantic-ai/openai-agents/crewai/adk/LangGraph), and durable-flow
  designation from `[flows]` — synchronous or async.
- **Node front end** (`node/keel`): loader + `fetch`/undici interception,
  AI-SDK middleware (all four ops) + eve integration, `mcp:`/`llm:` packs,
  `pg`/`ioredis`/`mysql2` database adapters, discovery, and durable-flow
  designation from `[flows]` (async-only, matching Node's own effect model).
- **CLI** (`crates/keel-cli`): `run · init · doctor (--effective-policy) ·
  status · explain · flows (suggest/add/resume) · trace · replay · tail ·
  fsck · mcp · record (run/list/test) · sim`, every command with a
  deterministic `--json` twin.
- **Tier 2**: durable flows in the core + both front ends, on SQLite or
  Postgres; `kill -9` a flow mid-run and `keel run` resumes it, replaying
  completed steps without re-firing their effects (proven by real subprocess
  `kill -9` tests in both languages) — including concurrent async effects
  inside one flow, which serialize deterministically in await order.
- **A scoped Rust front end** (`crates/keel` + `crates/keel-macros`):
  `#[keel::wrap(target = "...")]` plus a `reqwest-middleware` adapter — no
  `cargo-keel` subcommand or Rust static scanner yet (documented as
  deferred, not silently missing).

**Overhead:** the worst-case wrapped-call path measures **~0.8 µs**
against the **10 µs** budget (DX invariant 8), emitted as a CI artifact by
`scripts/bench-overhead.sh`.

**Honest packaging gap:** wheels (`pip install keel`), the npm package
(`npm i -D keel`), and the crate (`cargo add keel`) are **not published
yet** — today you build from source (below). The registry names `keel`/
`keel-core` collide with unrelated existing projects on npm/PyPI/crates.io;
see `docs/naming-decision.md` for the options. The native Python module
builds with `maturin`; the CLI with `cargo build`.

Licensed under [Apache-2.0](LICENSE).

**OTel export is opt-in and off by default.** The OTLP exporter lives behind the
`otel` cargo feature; the shipped wheel/addon carry no OpenTelemetry dependency.
To export the engine's `keel.call`/`keel.attempt` spans, build a native front end
with the feature and set `KEEL_OTEL=1` (the standard `OTEL_*` env vars configure
the endpoint) — e.g. `maturin develop -m crates/keel-py/Cargo.toml --features otel`
or `cargo build -p keel-node --release --features otel`. CI compiles `--features
otel` so it can't silently rot; end-to-end export against a live collector is a
manual verification step.

The guiding documents:

- [docs/architecture-spec.md](docs/architecture-spec.md) — what Keel is (two-tier semantics, core, journal, front ends)
- [docs/dx-spec.md](docs/dx-spec.md) — why anyone will love it (the product's soul; conflicts resolve in its favor)
- [docs/engineering-manifesto.md](docs/engineering-manifesto.md) — how we build it (repo standards, July 2026 SOTA)
- [docs/sprint-plan.md](docs/sprint-plan.md) — the parallel-team work breakdown, with a completion annotation
- [llms.txt](llms.txt) / [llms-full.txt](llms-full.txt) — the retrieval-built docs for coding agents

## Quickstart (from source)

**Python** (needs the Rust toolchain in `rust-toolchain.toml` + `maturin`):

```
maturin develop -m crates/keel-py/Cargo.toml     # builds the native keel_core into your venv
pip install -e 'python/keel[dev]'                # the front end + httpx/requests for the demos
keel run your_app.py                             # or: python -m keel run your_app.py
```

Without the native module the front end falls back to a pure-Python core (Tier 1
only; no persistent cache, no flows).

**Node** (≥ 22.5):

```
node node/keel/bin/keel-node-run.mjs your_app.mjs   # from-source `keel run` for Node
```

**CLI:**

```
cargo build -p keel-cli                # produces target/debug/keel
target/debug/keel init                 # write a keel.toml from evidence
target/debug/keel doctor --json        # the honesty report
```

## Demos

Four runnable, deterministic demos (no real network — [`tools/faultproxy`](tools/faultproxy)
serves scripted faults). See [demos/](demos) and the 40-second
[storyboard](demos/STORYBOARD.md).

| Demo | Proves | Lang | Native? |
|------|--------|------|---------|
| [flaky-python](demos/flaky-python) | 503 dies bare, `keel run` retries it | Python | no |
| [node-service](demos/node-service) | 500 dies bare, `keel run` retries it | Node | no |
| [agent-demo](demos/agent-demo) | 429-storm survived; dev-cache makes a 2nd run ~0 calls | Python | for replay |
| [durable-pipeline](demos/durable-pipeline) | `kill -9` mid-flow → resumes 10/10, each step once | Python | yes |

## Conformance

Every implementation interprets the same `conformance/scenarios/*.json` — green
here is the definition of done (normative semantics: `conformance/README.md`).

| Harness | Command | Result |
|---------|---------|--------|
| Real Rust core — Tier 1 | `cargo test -p keel-core --test conformance` | 21/21 (paused tokio clock) |
| Real Rust core — Tier 2 (SQLite) | `cargo test -p keel-core --test flows_conformance` | scenarios 16–27 |
| Real Rust core — Tier 2 (Postgres) | `cargo test -p keel-core --test flows_conformance_postgres` | a real scratch cluster, no docker |
| Rust stub | `cargo test -p keel-core-stub` | 21/21 (Tier 2 skipped: no journal) |
| Python stub | `python3 conformance/runner.py` | 21/21 (Tier 2 skipped) |
| Node stub | `cd node/keel-core-stub && node --test` | 21/21 (Tier 2 skipped) |
| Python native | `python3 conformance/runner.py --impl native` | 21/21 Tier 1 |

Scenarios 01–15 plus 18/19/20/21–22/23 are Tier 1 (the last five added
breaker rate-mode, token-bucket, schedule composition, and idempotency-key
injection); **16–27** are Tier 2 (resume/substitution, every nondeterminism
mode, lease contention, attempt caps, value-step replay, crash-mid-step
re-execution, pure replay, and replay under a changed policy), which need a
journal — so the stubs *and* the `runner.py` harness (both impls) skip them,
and the Tier 2 scenarios are driven by the real core's `flows_conformance`
(SQLite) and `flows_conformance_postgres` (Postgres) binaries. Tier 2
**through the native front ends** is covered by real subprocess `kill -9` +
resume tests in both `python/keel/tests/` and `node/keel/test/`. Plus:
`python3 conformance/check_schema.py` (policies vs. `policy.schema.json`) and
`python3 conformance/fixtures/journal/build_fixtures.py` (golden journal DBs).

## Repo layout

```
contracts/            frozen interfaces (policy schema, FFI, journal, adapter packs) — CCR to change
crates/
  keel-core-api/      contract types + shared typed policy model
  keel-core/          THE REAL CORE (Tier 1 + Tier 2 flows): tokio engine, journal, flows, events
  keel-journal/        SQLite + Postgres Journal implementations
  keel-ffi/           C-ABI facade (MessagePack) + async bridge
  keel-py/            PyO3 native module (keel_core), incl. the Tier-2 async flow bridge
  keel-node/          napi native addon, incl. the Tier-2 async flow bridge
  keel-cli/           the `keel` binary (run|init|doctor|status|explain|flows|trace|replay|
                        tail|fsck|mcp|record|sim), each with a scan/ static scanner (py, oxc-js)
  keel/                scoped Rust front end: #[keel::wrap] + reqwest-middleware
  keel-macros/          the #[keel::wrap] proc macro
  keel-core-stub/     in-memory reference stub (Rust); keel-conformance/ shared harness
python/keel/          Python front end (import hook, adapters, packs, flows, record, sim)
python/keel-core-stub/  the same stub, pure Python
node/keel/            Node front end (loader, fetch, packs, flows, record, sim)
node/keel-core-stub/    the same stub, pure Node
conformance/          scenario matrix + runners; fixtures/journal/ golden DBs
demos/                four runnable demos + the storyboard
tools/faultproxy/     scriptable deterministic fault-injecting proxy (demos, tests, keel sim)
docs/                 the specs, sprint plan, CCRs, and the recording/sim/targeting format docs
```

## How work proceeds

Teams build against the stub and the frozen contracts (see
[contracts/README.md](contracts/README.md) for the change process); the stub was
swapped for the real Rust core on integration day, with conformance the referee
at every step. The full Sprint 0–2 plan and what shipped against it are in
[docs/sprint-plan.md](docs/sprint-plan.md). Session context for coding agents
lives in [CLAUDE.md](CLAUDE.md); the agent-facing docs are `llms.txt` /
`llms-full.txt`, and `keel init --agents` drops a Keel section into `AGENTS.md`.
MCP-native agents can run `keel mcp`: the CLI doubles as an MCP server over
stdio (no daemon — it exits on EOF), exposing `get_status`, `get_doctor_report`,
`propose_policy` (a keel.toml diff), `get_trace`, `list_flows`, and
`explain_error`, each byte-identical to the matching `--json` command.
