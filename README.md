# Keel

[![CI](https://github.com/MisterTK/keel/actions/workflows/ci.yml/badge.svg)](https://github.com/MisterTK/keel/actions/workflows/ci.yml)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

**The SQLite of durable execution.** Production-grade resilience and
crash-resumable workflows, running *inside* your process — no service to
deploy, no database to provision, no code to rewrite.

```
$ keel run app.py
keel ▸ wrapped 14 call sites (httpx ×9, openai ×4, psycopg ×1) with production defaults — `keel init` to customize
```

That's it. Your outbound HTTP calls, database queries, and LLM requests now
retry on transient failures, back off exponentially, trip a circuit breaker
under sustained failure, respect rate limits, and — if you opt in — survive a
crash and resume exactly where they left off. Your code is untouched.
Uninstall the package and you're back to exactly what you had before.

## The problem

Every service that talks to the network eventually gets paged for the same
handful of reasons: a downstream dependency blipped and nothing retried it, a
retry storm took down a service that would have recovered on its own, or a
long-running job died halfway through and had to restart from zero. Fixing
this yourself means scattering retry decorators across a codebase and hoping
every new call site remembers them. Fixing it "properly" usually means
adopting a workflow engine — a service to run, a database to provision, and a
rewrite of business logic into activities and workflows — for problems that
were never distributed-systems-scale to begin with.

Keel takes neither path. It's a library, not a service: it intercepts calls
you're already making, applies policy from one `keel.toml` file, and — only
when a call needs to survive a crash — journals it to a local file. No
daemon. No port. No new abstractions in your code.

## Why Keel

|  | Hand-rolled retry decorators | Workflow engines (Temporal-style) | **Keel** |
|---|---|---|---|
| Code changes | Decorate every call site | Rewrite as activities/workflows | **Zero** |
| Infrastructure | None | A service, a database, a cluster | **None** — a local file |
| Consistency across a codebase | Whatever each engineer remembers | Enforced by the framework | Enforced by one policy file |
| Crash-resumable execution | No | Yes | **Opt-in**, same library |
| Removing it | Undo the decorators, one by one | A migration project | Uninstall the package |

## What you get

- **Zero code changes.** Keel patches the interception seams your language
  already exposes (Python's import hooks, Node's ESM loader, an attribute
  macro for Rust) — your source is never touched, and uninstalling the
  package restores your original behavior exactly.
- **Production-grade defaults, out of the box.** Every discovered outbound
  call gets a 30s timeout, 3 retries with jittered exponential backoff on
  transient errors, and a per-host circuit breaker — before you write a
  single line of config.
- **One `keel.toml`, not a decorator per call site.** Retry schedules,
  timeouts, rate limits, caching, and circuit breakers are policy, not code
  — reviewed like infrastructure, not scattered through business logic.
- **Opt-in durable execution.** Designate a function as a flow and its steps
  are journaled: `kill -9` it mid-run, and rerunning it replays completed
  steps from the journal instead of re-executing their side effects —
  proven by real subprocess crash-and-resume tests, not a mocked clock.
- **Observable when you need it, invisible when you don't.** OpenTelemetry
  spans and metrics for every call and attempt are one build feature and one
  env var away — off by default, so the shipped library carries no
  OpenTelemetry dependency until you ask for it.
- **Built for LLM and agent workloads.** First-class `llm:`/`tool:`/`mcp:`
  targets, per-run spend caps, model fallback chains, and a dev-mode cache
  that replays identical prompts for free — because agent code is the
  densest concentration of flaky, expensive effects in modern software.
- **Fast enough to be invisible.** The wrapped-call path measures ~0.8µs
  worst case against a 10µs budget — resilience you can't feel.
- **Agent-native tooling.** `keel mcp` serves the CLI itself as an MCP
  server; every command has a deterministic `--json` twin; `keel explain
  <code>` gives a coding agent the exact remedy without a web search.
- **Two languages today, checked against each other.** Python and
  Node/TypeScript both run on the same real, tested Rust core; a scoped
  Rust front end covers `#[keel::wrap]`-annotated functions directly. Every
  implementation is checked against the same conformance suite — the tests
  are the spec, not the docs.

## Quickstart

Keel isn't published to a package registry yet (see [Status](#status)) —
building from source takes about a minute.

**Python** (needs Rust — `rustup` picks up the pinned toolchain
automatically):

```bash
maturin develop -m crates/keel-py/Cargo.toml     # builds the native core into your venv
pip install -e 'python/keel[dev]'
keel run your_app.py                             # or: python -m keel run your_app.py
```

Without the native module, the front end falls back to a pure-Python core:
Tier 1 resilience still works, but there's no persistent cache and no
durable flows.

**Node** (≥ 22.5):

```bash
node node/keel/bin/keel-node-run.mjs your_app.mjs
```

**CLI only:**

```bash
cargo build -p keel-cli
target/debug/keel init            # generate keel.toml from evidence: imports, call sites, observed traffic
target/debug/keel doctor --json   # the honesty report — what's covered, what isn't, why
```

## See it work

Four runnable, deterministic demos — no real network involved
([`tools/faultproxy`](tools/faultproxy) serves scripted faults). See the
40-second [storyboard](demos/STORYBOARD.md) for the shooting script.

| Demo | What it proves | Language |
|------|-----------------|----------|
| [flaky-python](demos/flaky-python) | A bare script dies on a 503; `keel run` survives it | Python |
| [node-service](demos/node-service) | Same story, Node: a bare script dies on a 500; `keel run` survives it | Node |
| [agent-demo](demos/agent-demo) | An LLM call survives a 429 storm; a second run costs ~0 API calls (dev cache) | Python |
| [durable-pipeline](demos/durable-pipeline) | `kill -9` mid-flow, rerun, and it resumes 10/10 steps — each firing exactly once | Python |

## How it works

Two tiers, one policy file:

- **Tier 1 — resilience.** Every intercepted call passes through a fixed
  layer chain: cache → rate limit → circuit breaker → timeout → retry.
  Stateless, works everywhere, needs nothing but the library.
- **Tier 2 — durable flows (opt-in).** Designate an entrypoint in `[flows]`
  and its steps are journaled to a local SQLite file (or Postgres, for
  fleet deployments) as they run. A crash — or a deliberate restart —
  replays completed steps from the journal instead of re-firing their side
  effects, then resumes live from wherever it left off.

Both tiers run on the same native Rust core via a C ABI, so the Python and
Node front ends share identical semantics — verified by a shared
[conformance suite](conformance/README.md) that every implementation must
pass, not just documentation asserting it.

## Status

Keel is pre-1.0 and not yet published to any package registry (`pip`/`npm`/
`cargo`/`brew` — see the from-source [Quickstart](#quickstart) above; a
package-naming decision is pending). Everything described in this README is
real, tested, and running on the native core in both languages today — this
isn't a roadmap, it's what's built. What's explicitly *not* built yet: a
zero-config Rust CLI wrapper (Rust requires the `#[keel::wrap]` attribute
instead), custom regex retry conditions, an object-store-backed journal for
massive scale, and a hermetic/WASM simulation mode.

Bug reports and pull requests are welcome — open an issue or a PR.

## Learn more

- [`llms.txt`](llms.txt) / [`llms-full.txt`](llms-full.txt) — compact,
  retrieval-friendly docs for coding agents evaluating or integrating Keel.
- [`conformance/README.md`](conformance/README.md) — the normative
  behavior every implementation is tested against.
- [`contracts/README.md`](contracts/README.md) — the frozen interfaces
  (policy schema, FFI, journal, adapter-pack contract) and how they change.
- [`python/keel/README.md`](python/keel/README.md) /
  [`node/keel/README.md`](node/keel/README.md) — full front-end reference
  for each language.

Licensed under [Apache-2.0](LICENSE).
