# Keel — session context

Keel is "the SQLite of durable execution": resilience (retry/backoff/timeout/
breaker/rate/cache) and opt-in durable flows as an in-process library, zero
code changes, policy in one `keel.toml`. Source-of-truth documents, in
authority order when they conflict:

1. `docs/architecture-spec.md` — what Keel is (two-tier semantics is the load-bearing idea)
2. `docs/dx-spec.md` — the product's soul; every conflicting engineering decision loses to it
3. `docs/engineering-manifesto.md` — how we build (standards, July 2026 SOTA)
4. `docs/sprint-plan.md` — the team/work breakdown being followed

## Where the codebase actually is (2026-07-13, post weekend-sprint + 3-wave completion program)

The weekend sprint (17 tasks) landed first, then a follow-on multi-wave
program (branch `claude/complete-keel`, ~50k lines across 300+ files) closed
almost every gap between that sprint and the full architecture/DX specs.
Both languages now have **Tier 1 and Tier 2 (including async flows) on the
real native core**, a Postgres journal backend, a much larger adapter/pack
surface, and a scoped Rust front end. Done:

- **Sprint 0 complete, `contracts-v1` frozen**: `contracts/` (policy JSON
  Schema, schedule EBNF, smart-defaults pack, `core-ffi.h` C ABI,
  `core_api.rs` envelope types, KEEL-E0NN error taxonomy, `journal.sql`,
  adapter-pack contract in 3 languages). Changes need a CCR + the
  `contract-change-approved` PR label or CI fails. Two CCRs approved so far:
  PR #5 (`135580d` — KEEL-E005 absorbs the v0.1 "valid policy, missing
  capability" cases; `keel_configure` receives the effective policy) and
  CCR-2 (`docs/ccr/0002-async-steps-and-idempotency-injection.md` — retires
  the async-in-flow KEEL-E005 refusal now that both bindings have a real
  async bridge, and specifies idempotency-key injection semantics in
  `contracts/adapter-pack.md`).
- **keel-core-stub in three languages** (Rust `crates/keel-core-stub`,
  Python `python/keel-core-stub`, Node `node/keel-core-stub`) with
  bit-identical documented semantics on a virtual clock — Tier 1 only; Tier 2
  is real-core-only (stubs skip `"tier": 2` scenarios, per
  `conformance/README.md`).
- **Conformance suite**: 27 scenarios in `conformance/scenarios/` (01–15 Tier 1
  plus later Tier 1 additions for breaker rate-mode/token-bucket/schedule
  composition/idempotency injection at 18–19/20/21–22/23, 16–27 Tier 2). Real
  Rust core passes all of them (Tier 2 additionally exercised against a real
  Postgres backend, `crates/keel-core/tests/flows_conformance_postgres.rs`);
  stubs pass every Tier 1 scenario, skip Tier 2. Normative semantics —
  including the async-flow-step ordering rule, schedule algebra, and
  idempotency injection — live in `conformance/README.md`. Golden journal
  fixtures in `conformance/fixtures/journal/`.
- **The real core** (`crates/keel-core`): async tokio Engine, layer chain
  cache → rate (token-bucket) → breaker (count or failure-rate mode) →
  timeout → retry (with `upTo`/`andThen` schedule composition), per-attempt
  timeouts (KEEL-E011), equal jitter, idempotency-key-injection awareness;
  a Tier-1 NDJSON event sink (`crates/keel-core/src/events.rs`,
  `.keel/events/*.ndjson`, feeds `keel tail`); **Tier 2 durable flows**
  (`flow.rs`: journaled steps, resume, replay substitution, nondeterminism
  defense (fail/warn/branch), leases, time/random virtualization, and now an
  **async execute_step bridge** used by both Python and Node bindings —
  concurrent async effects inside one open flow serialize in await/admission
  order, normatively documented in `conformance/README.md`).
- **Journal backends** (`crates/keel-journal`): SQLite (WAL, the default) and
  a real **PostgresJournal** (Level 3 / fleet durability — DB-clock-based
  lease arbitration, a dedicated-OS-thread connection pool to avoid the sync
  `postgres` crate panicking inside keel-core's own Tokio runtime, schema at
  `crates/keel-journal/src/postgres_schema.sql`, not in `contracts/`).
  Selection is via the policy `journal` key or `KEEL_JOURNAL`
  (`crates/keel-core/src/journal_backend.rs`); `keel doctor`/`keel fsck`
  report on it.
- **FFI + bindings**: `crates/keel-ffi` (C ABI, MessagePack) with PyO3
  (`crates/keel-py` → `keel_core`) and napi (`crates/keel-node`) async
  bridges, both including the Tier-2 async flow surface
  (enter/exit/journal_time/journal_random plus the serializing execute_step
  bridge).
- **Front ends**: `python/keel` and `node/keel`, now at rough feature parity:
  import hook / ESM loader, httpx+requests+aiohttp+urllib3+boto3+psycopg
  (Python) and fetch/undici+pg+ioredis+mysql2 (Node) adapters, `llm:` packs
  (openai/anthropic/google-genai) + dev cache, `tool:`/`mcp:` semantic
  target classes, agent-framework packs (pydantic-ai/openai-agents/crewai/adk
  in Python; AI-SDK four-ops+eve in Node; LangGraph wrapping +
  journal-backed checkpointer in Python), LLM budget caps + model fallback
  chains, host/URL-pattern outbound targeting (`docs/targeting.md`), Tier-2
  flow designation (`[flows] entrypoints`, both languages now, including the
  async bridge), `keel record`/`keel sim` front-end hooks
  (`docs/recording-format.md`, `docs/sim-format.md`). Both run on the native
  core, falling back to the pure stub when it is absent.
- **CLI** (`crates/keel-cli`): `run · init · doctor (--effective-policy) ·
  status · explain · flows (suggest/add/resume) · trace · replay · tail ·
  fsck · mcp · record (run/list/test) · sim`, every command with a
  deterministic `--json` twin (golden-tested); `keel init --agents` seeds
  `AGENTS.md`; `keel mcp` serves six tools over stdio JSON-RPC, each
  byte-identical to its `--json` CLI twin.
- **A scoped Rust front end** (`crates/keel` + `crates/keel-macros`):
  `#[keel::wrap(target = "...")]` reads `keel.toml` at compile time and
  routes a function through the Engine chain; a `reqwest-middleware`
  adapter for `host:` targets (sends attempts directly rather than
  delegating to the middleware chain — see the crate's module docs for the
  `Send`-across-`#[async_trait]` limitation this works around). Deliberately
  NOT built: a `cargo-keel` subcommand, a `syn`-based Rust static scanner,
  `keel init --rust` — documented architectural debt, not silently dropped.
- **Overhead**: worst-case wrapped-call path ~0.8 µs vs the 10 µs budget, even
  with the event sink and idempotency injection added
  (`scripts/bench-overhead.sh` → `target/bench-overhead.json`).
- **JS/TS static scan**: real oxc AST parse (`crates/keel-cli/src/scan/js/`),
  not a line heuristic — handles class methods, arrows, JSX, multi-line
  imports, template literals, and per-function effect/time/random/unsafe-
  construct attribution for `keel flows suggest`.
- **Release infra** (not yet published — see below): `.github/workflows/
  release.yml` (wheels via maturin, napi prebuilds, CLI binaries incl. Linux
  musl, a homebrew formula renderer), `.github/workflows/adapter-farm.yml`
  (adapter contract tests against pinned real library versions), `deny.toml`
  + `scripts/check-licenses.{py,mjs}` (NFR6 license gate), `scripts/
  check-versions.py` / `bump-version.sh` (single version source),
  `scripts/check-release-metadata.sh`.
- **Demos + tooling**: `demos/` (flaky-python, node-service, agent-demo,
  durable-pipeline) each with `run.sh` + a smoke test; `tools/faultproxy`
  (deterministic fault-injecting proxy, also reused by `keel sim`).
  Retrieval docs: `llms.txt` / `llms-full.txt`.
- Rust 1.97.0 pinned (`rust-toolchain.toml`), edition 2024, clippy-pedantic
  `-D warnings`, `Cargo.lock` committed, CI green.

**Honest gaps, still real**: nothing is actually *published* — wheels, the
npm package, the crates, and the brew formula all build from source or CI
artifacts only (`docs/naming-decision.md`: the plain names `keel`/`keel-core`
are taken on npm/PyPI/crates.io by unrelated projects; a naming decision is
needed before any registry publish, independent of the code being ready).
Deliberately deferred, documented as debt rather than silently missing:
the Tier 2 flow surface is not exposed over the frozen C ABI (Python/Node
reach it via direct Rust-crate bindings, not `contracts/core-ffi.h` — a CCR
if ever needed); custom regex retry conditions (`contracts/policy.schema.json`
would need extending — a CCR); an object-store-backed segmented-log journal
(architecture-spec's "massive scale, later" tier); hermetic/WASM simulation
mode (architecture-spec §8, gated on WASI-0.3 toolchain maturity); a
`cargo-keel` subcommand + Rust static scanner + `keel init --rust` (the Rust
front end is intentionally scoped to `#[keel::wrap]` + reqwest-middleware
only, see above).
Licensed **Apache-2.0** (decided by TK 2026-07-12; `LICENSE` at repo root, SPDX
fields in every manifest). **OTel export is opt-in**: spans AND metrics are
behind the `otel` cargo feature (off by default; the shipped wheel/addon have
no OpenTelemetry dep) and only initialize when a front end is built
`--features otel` (forwarded by `keel-py`/`keel-node` → `keel-core/otel`)
*and* `KEEL_OTEL=1` is set (`OTEL_*` env configures the exporter). CI's
`otel-build` job compiles it; live collector export is a manual step.

## Commands (run all before any push that touches behavior)

```bash
# rustup, not Homebrew cargo, or the rust-version=1.97 gate fails outright:
export PATH="$HOME/.rustup/toolchains/1.97.0-aarch64-apple-darwin/bin:$PATH"

cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --all --locked          # real core + stub conformance + CLI + tier-2 (incl. real
                                    # Postgres integration tests via a local scratch cluster —
                                    # needs postgresql@15's initdb/pg_ctl/psql on PATH)
python3 conformance/runner.py                    # Python stub: 21/21 expected (13 tier-2 skipped)
(cd node/keel-core-stub && node --test)          # Node stub: same
python3 conformance/check_schema.py              # needs `pip install jsonschema`
python3 conformance/fixtures/journal/build_fixtures.py
python3 tools/faultproxy/test_faultproxy.py      # faultproxy sequencing
python3 scripts/check-licenses.py                # zero-runtime-deps + dev-dep license allowlist
node scripts/check-licenses.mjs                  # same, Node side
python3 scripts/check-versions.py                # single version source, all manifests agree
bash scripts/check-release-metadata.sh           # cargo package listings + manifest invariants
(cd python/keel && python3 -m unittest discover) # Python front end (stub; native legs if keel_core built)
(cd node/keel && node --test)                    # Node front end
```

Native front-end legs (Tier 2 flows, persistent cache, native adapters) need
the built module: `maturin develop --uv -m crates/keel-py/Cargo.toml` (Python
— test against it with the repo's own `.venv/bin/python -m unittest discover`,
**not** just plain `python3`, which has a different site-packages and can mask
import-time bugs — see the `google` namespace gotcha in `keel.packs._provider.
module_present`'s docstring), `cargo build -p keel-node --release` (Node;
`node/keel-core-native` loads the built cdylib); they skip cleanly otherwise.
Overhead artifact: `bash scripts/bench-overhead.sh` (worst case ~0.8 µs vs the
10 µs budget as of this writing).

## Rules that bite

- `contracts/` is frozen — CCR process in `contracts/README.md`; two CCRs
  approved so far (PR #5, CCR-2 — see above). Any PR touching `contracts/`
  needs the `contract-change-approved` label or CI fails, and after any edit
  there run `scripts/sync-vendored.sh` (the publishable crates vendor
  `contracts/` in, checked by `check-release-metadata.sh`).
- Tier 1 stub/core/Python/Node semantics must stay identical; change them
  only by changing `conformance/README.md` + all implementations + scenarios
  together. Tier 2 (durable flows) is real-core-only by design — the three
  stubs skip every `"tier": 2` scenario and never need Tier 2 changes.
- Work happens on a session branch with a draft PR into `main`; never push
  other branches without explicit permission. The current work branch is
  `claude/complete-keel` (a multi-wave program closing the gap to the full
  specs — see the section above); history through 2026-07-12 landed via
  PRs #1–#5 from earlier sessions.
- Follow `docs/engineering-manifesto.md` for anything not covered here —
  typed-struct validation, `#[expect(reason)]` over `allow`, no locks across
  `await`, no real sleeps in tests, deterministic output.
- Plain `cargo` on this machine resolves to Homebrew's 1.96.1, which fails
  the workspace's `rust-version = 1.97` gate outright — always prefix
  `PATH="$HOME/.rustup/toolchains/1.97.0-aarch64-apple-darwin/bin:$PATH"`.
  Piping a long `cargo test`/`cargo build` directly into `head`/`tail` under
  `set -o pipefail` can SIGPIPE it and report a false failure — redirect to a
  log file first, then grep/tail the file.
