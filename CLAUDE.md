# Keel — session context

Keel is "the SQLite of durable execution": resilience (retry/backoff/timeout/
breaker/rate/cache) and opt-in durable flows as an in-process library, zero
code changes, policy in one `keel.toml`. Source-of-truth documents, in
authority order when they conflict:

1. `docs/architecture-spec.md` — what Keel is (two-tier semantics is the load-bearing idea)
2. `docs/dx-spec.md` — the product's soul; every conflicting engineering decision loses to it
3. `docs/engineering-manifesto.md` — how we build (standards, July 2026 SOTA)
4. `docs/sprint-plan.md` — the team/work breakdown being followed

## Where the codebase actually is (2026-07-11)

Done, in `main`/PR #1 (branch `claude/new-session-3p7yyq`):

- **Sprint 0 complete, `contracts-v1` frozen**: `contracts/` (policy JSON
  Schema, schedule EBNF, smart-defaults pack, `core-ffi.h` C ABI,
  `core_api.rs` envelope types, KEEL-E0NN error taxonomy, `journal.sql`,
  adapter-pack contract in 3 languages). Changes need a CCR + the
  `contract-change-approved` PR label or CI fails.
- **keel-core-stub in three languages** (Rust `crates/keel-core-stub`,
  Python `python/keel-core-stub`, Node `node/keel-core-stub`) with
  bit-identical documented semantics on a virtual clock.
- **Conformance suite**: 15 scenarios in `conformance/scenarios/`,
  interpreted by four harnesses (real core, Rust/Python/Node stubs) — all
  15/15. Normative semantics: `conformance/README.md`. Golden journal
  fixtures in `conformance/fixtures/journal/`.
- **The real core, first slice** (`crates/keel-core`): async tokio Engine,
  layer chain cache → rate → breaker → timeout → retry, typed policy model
  shared via `keel_core_api::policy`, enforced per-attempt timeouts
  (KEEL-E011), equal jitter. Passes the same 15 scenarios under
  `start_paused` virtual time.
- Rust 1.97.0 pinned (`rust-toolchain.toml`), edition 2024,
  clippy-pedantic `-D warnings`, `Cargo.lock` committed, CI green
  (rust / python / node / contract-freeze).

NOT built yet (Team A next slices, then other teams): SQLite discovery
journal, OTel spans, FFI facade + PyO3/napi async bridge, ≤10µs overhead
benchmark; Python front end (`keel run`, import hook, httpx/requests
adapters); Node front end; CLI (`keel run|init|doctor|status|explain`);
LLM/agent-framework packs; Tier 2 durable flows.

## Commands (run all before any push that touches behavior)

```bash
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --all --locked                        # incl. real-core + stub conformance
python3 conformance/runner.py                    # Python stub: 15/15 expected
(cd node/keel-core-stub && node --test)          # Node stub: 15/15 expected
python3 conformance/check_schema.py              # needs `pip install jsonschema`
python3 conformance/fixtures/journal/build_fixtures.py
```

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
