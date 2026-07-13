# Keel — session context

Keel is "the SQLite of durable execution": resilience (retry/backoff/timeout/
breaker/rate/cache) and opt-in durable flows as an in-process library, zero
code changes, policy in one `keel.toml`. Source-of-truth documents, in
authority order when they conflict:

1. `docs/architecture-spec.md` — what Keel is (two-tier semantics is the load-bearing idea)
2. `docs/dx-spec.md` — the product's soul; every conflicting engineering decision loses to it
3. `docs/engineering-manifesto.md` — how we build (standards, July 2026 SOTA)
4. `docs/sprint-plan.md` — the team/work breakdown being followed

## Where the codebase actually is (2026-07-12, post-sprint)

All 17 sprint tasks landed. Tier 1 works end-to-end in **both languages on the
real native core**, plus the CLI and **Tier 2 durable flows with `kill -9`
resume**. Done:

- **Sprint 0 complete, `contracts-v1` frozen**: `contracts/` (policy JSON
  Schema, schedule EBNF, smart-defaults pack, `core-ffi.h` C ABI,
  `core_api.rs` envelope types, KEEL-E0NN error taxonomy, `journal.sql`,
  adapter-pack contract in 3 languages). Changes need a CCR + the
  `contract-change-approved` PR label or CI fails.
- **keel-core-stub in three languages** (Rust `crates/keel-core-stub`,
  Python `python/keel-core-stub`, Node `node/keel-core-stub`) with
  bit-identical documented semantics on a virtual clock.
- **Conformance suite**: 17 scenarios in `conformance/scenarios/` (01–15 Tier 1,
  16–17 Tier 2). Real Rust core 17/17; Rust/Python/Node stubs 15/15 (Tier 2
  skipped — no journal); native front ends run Tier 2. Normative semantics:
  `conformance/README.md`. Golden journal fixtures in
  `conformance/fixtures/journal/`.
- **The real core** (`crates/keel-core`): async tokio Engine, layer chain
  cache → rate → breaker → timeout → retry, per-attempt timeouts (KEEL-E011),
  equal jitter; SQLite discovery + flow journal (`crates/keel-journal`), OTel
  spans, and **Tier 2 durable flows** (`flow.rs`: journaled steps, resume,
  replay substitution, nondeterminism defense, leases, time/random
  virtualization).
- **FFI + bindings**: `crates/keel-ffi` (C ABI, MessagePack) with PyO3
  (`crates/keel-py` → `keel_core`) and napi (`crates/keel-node`) async bridges.
- **Front ends**: `python/keel` (import hook, httpx + requests adapters,
  `llm:` packs + dev cache, durable-flow designation) and `node/keel` (loader,
  fetch/undici, AI-SDK middleware, `mcp:`/`llm:` packs). Both run on the native
  core, falling back to the pure stub when it is absent.
- **CLI** (`crates/keel-cli`): `run · init · doctor · status · explain · flows ·
  trace`, every command with a deterministic `--json` twin (golden-tested);
  `keel init --agents` seeds `AGENTS.md`.
- **Overhead**: worst-case wrapped-call path ~0.56 µs vs the 10 µs budget
  (`scripts/bench-overhead.sh` → `target/bench-overhead.json`).
- **Demos + tooling**: `demos/` (flaky-python, node-service, agent-demo,
  durable-pipeline) each with `run.sh` + a smoke test; `tools/faultproxy`
  (deterministic fault-injecting proxy). Retrieval docs: `llms.txt` /
  `llms-full.txt`.
- Rust 1.97.0 pinned (`rust-toolchain.toml`), edition 2024, clippy-pedantic
  `-D warnings`, `Cargo.lock` committed, CI green.

**Honest gaps**: wheels (`pip install keel`) and the npm package are not
published — build from source (README quickstart; `maturin develop -m
crates/keel-py` for the native Python module). Postgres/fleet journal (Level 3),
`keel mcp`, `keel record test`, and further adapter packs are future work.
An approved CCR (PR #5, `135580d`) resolved two contract
questions: **KEEL-E005 unsupported-configuration** now carries the v0.1
"valid policy, missing capability" cases (async-in-flow, flows-without-journal,
`KEEL_BACKEND=native` with no native module), so KEEL-E001 is pure policy
validation again; and `keel_configure` is contractually given the **effective
policy** — front ends/CLI compose defaults < packs < user *before* calling the
core, which layers no pack underneath (`keel doctor --effective-policy` is
follow-up: the Rust CLI has no merge implementation yet).
Licensed **Apache-2.0** (decided by TK 2026-07-12; `LICENSE` at repo root, SPDX
fields in every manifest). **OTel export is opt-in**: the OTLP exporter
is behind the `otel` cargo feature (off by default; the shipped wheel/addon have no
OpenTelemetry dep) and only initializes when a front end is built `--features otel`
(forwarded by `keel-py`/`keel-node` → `keel-core/otel`) *and* `KEEL_OTEL=1` is set
(`OTEL_*` env configures the exporter). CI's `otel-build` job compiles it; live
collector export is a manual step.

## Commands (run all before any push that touches behavior)

```bash
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --all --locked                        # real core + stub conformance + CLI + tier-2
python3 conformance/runner.py                    # Python stub: 15/15 expected
(cd node/keel-core-stub && node --test)          # Node stub: 15/15 expected
python3 conformance/check_schema.py              # needs `pip install jsonschema`
python3 conformance/fixtures/journal/build_fixtures.py
python3 tools/faultproxy/test_faultproxy.py      # faultproxy sequencing
(cd python/keel && python3 -m unittest discover) # Python front end (stub; native legs if keel_core built)
(cd node/keel && node --test)                    # Node front end
```

Native front-end legs (flows, persistent cache, native adapters) need the built
module: `maturin develop -m crates/keel-py/Cargo.toml` (Python), `cargo build
-p keel-node --release` (Node; `node/keel-core-native` loads the built cdylib);
they skip cleanly otherwise. Overhead artifact:
`bash scripts/bench-overhead.sh`.

## Rules that bite

- `contracts/` is frozen — CCR process in `contracts/README.md`.
- Stub/core/Python/Node semantics must stay identical; change them only by
  changing `conformance/README.md` + all implementations + scenarios together.
- Work happens on the session's designated branch with a draft PR into
  `main`; never push other branches without explicit permission. (History
  through 2026-07-11 landed via PR #1 from `claude/new-session-3p7yyq` —
  make sure it's merged before branching new work off `main`.)
- Follow `docs/engineering-manifesto.md` for anything not covered here —
  typed-struct validation, `#[expect(reason)]` over `allow`, no locks across
  `await`, no real sleeps in tests, deterministic output.
