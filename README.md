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

## Status (2026-07-12)

Tier 1 works end-to-end in **both languages** on the **real native core**, plus
the CLI and **Tier 2 durable flows with crash resume**:

- **Real Rust core** (`crates/keel-core`): async tokio engine, layer chain
  cache → rate → breaker → timeout → retry, per-attempt timeouts, equal-jitter
  backoff, SQLite discovery + flow journal, OTel spans, and an FFI facade
  (`crates/keel-ffi`, MessagePack over a C ABI) with PyO3 + napi async bridges.
- **Python front end** (`python/keel`): `keel run`, import-hook wrapping, httpx +
  requests adapters, `llm:openai`/`llm:anthropic` packs with the dev cache, and
  durable-flow designation from `[flows]`.
- **Node front end** (`node/keel`): loader + `fetch`/undici interception,
  AI-SDK middleware, `mcp:`/`llm:` packs, discovery.
- **CLI** (`crates/keel-cli`): `run · init · doctor · status · explain · flows ·
  trace`, every command with a deterministic `--json` twin.
- **Tier 2**: durable flows in the core + Python front end; `kill -9` a flow
  mid-run and `keel run` resumes it, replaying completed steps without re-firing
  their effects (proven by a real subprocess `kill -9` test).

**Overhead:** the worst-case wrapped-call path (cache-miss) measures **~0.56 µs**
against the **10 µs** budget (DX invariant 8), emitted as a CI artifact by
`scripts/bench-overhead.sh`.

**Honest packaging gap:** wheels (`pip install keel`) and the npm package
(`npm i -D keel`) are **not published yet** — today you build from source (below).
The native Python module builds with `maturin`; the CLI with `cargo build`.

**License is undecided** — the choice belongs to TK ([architecture-spec §10](docs/architecture-spec.md)); until it lands, treat the code as all-rights-reserved (see [LICENSE-PENDING.md](LICENSE-PENDING.md)).

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
| Real Rust core — Tier 1 | `cargo test -p keel-core --test conformance` | 15/15 (paused tokio clock) |
| Real Rust core — Tier 2 | `cargo test -p keel-core --test flows_conformance` | scenarios 16–17 |
| Rust stub | `cargo test -p keel-core-stub` | 15/15 (2 Tier 2 skipped: no journal) |
| Python stub | `python3 conformance/runner.py` | 15/15 (2 Tier 2 skipped) |
| Node stub | `cd node/keel-core-stub && node --test` | 15/15 (2 Tier 2 skipped) |
| Python native | `python3 conformance/runner.py --impl native` | 15/15 Tier 1 |

Scenarios 01–15 are Tier 1; **16–17** are Tier 2 (flow resume/substitution and
`fail`-mode nondeterminism), which need a journal — so the stubs *and* the
`runner.py` harness (both impls) skip them, and the Tier 2 scenarios are driven
by the real core's `flows_conformance` binary. Tier 2 **through the native front
end** is covered by `python/keel/tests/test_resume_demo.py` (real `kill -9` +
resume) and `test_flows.py`. Plus: `python3 conformance/check_schema.py`
(policies vs. `policy.schema.json`) and
`python3 conformance/fixtures/journal/build_fixtures.py` (golden journal DBs).

## Repo layout

```
contracts/            frozen interfaces (policy schema, FFI, journal, adapter packs) — CCR to change
crates/
  keel-core-api/      contract types + shared typed policy model
  keel-core/          THE REAL CORE (Tier 1 + Tier 2 flows): tokio engine, journal, flows
  keel-ffi/           C-ABI facade (MessagePack) + async bridge
  keel-py/            PyO3 native module (keel_core)
  keel-node/          napi native addon
  keel-cli/           the `keel` binary (run|init|doctor|status|explain|flows|trace)
  keel-core-stub/     in-memory reference stub (Rust); keel-conformance/ shared harness
python/keel/          Python front end (import hook, adapters, llm packs, flows)
python/keel-core-stub/  the same stub, pure Python
node/keel/            Node front end (loader, fetch, ai-sdk/mcp/llm packs)
node/keel-core-stub/    the same stub, pure Node
conformance/          scenario matrix + runners; fixtures/journal/ golden DBs
demos/                four runnable demos + the storyboard
tools/faultproxy/     scriptable deterministic fault-injecting proxy (demos + tests)
docs/                 the specs + sprint plan
```

## How work proceeds

Teams build against the stub and the frozen contracts (see
[contracts/README.md](contracts/README.md) for the change process); the stub was
swapped for the real Rust core on integration day, with conformance the referee
at every step. The full Sprint 0–2 plan and what shipped against it are in
[docs/sprint-plan.md](docs/sprint-plan.md). Session context for coding agents
lives in [CLAUDE.md](CLAUDE.md); the agent-facing docs are `llms.txt` /
`llms-full.txt`, and `keel init --agents` drops a Keel section into `AGENTS.md`.
