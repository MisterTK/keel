# keel (Python front end)

Production-grade resilience for any Python program, with **zero code changes**.
`keel run app.py` intercepts your outbound calls (httpx, requests, aiohttp,
urllib3, boto3, psycopg, LLM SDKs via `llm:` packs, `tool:`/`mcp:` targets,
agent-framework packs, and `py:` function targets) and applies retries, backoff,
timeouts, circuit breakers, rate limits, and an optional dev-mode response
cache — all declared in one `keel.toml`, enforced by the native Rust core
inside your process. No daemon, no port, no login.

```
$ cd python/keel && pip install -e . && keelrun-py-run app.py
keel ▸ wrapped 14 call sites (httpx ×9, openai ×4) with production defaults — `keel init` to customize
```

(Not yet published to any registry; run from source. Published name will be
`keelrun` — see `docs/naming-decision.md`.)

Uninstalling Keel removes the behavior and nothing else: your code runs
identically (minus resilience). No imports, no context objects, no base classes.

## Backends

Keel resolves a backend at startup (`KEEL_BACKEND=auto|native|stub`):

- **native** (`keel_core`) — the PyO3 module bundling the Rust core. Required
  for the persistent dev cache and for Tier 2 durable flows. Built from
  `crates/keel-py` (`maturin develop`); prebuilt wheels are not published yet.
- **stub** — a pure-Python core (the conformance reference). Tier 1 semantics
  only; no journal, so no persistent cache and no flows.

`auto` (the default) uses the native core when importable, else the stub.

## Tier 2 — durable flows (Level 2)

Designate an entrypoint in `keel.toml` and `keel run` executes it as a durable
flow: every intercepted call inside is journaled, and a rerun after a crash
substitutes already-completed steps from the journal instead of re-firing them.

```toml
[flows]
entrypoints = ["py:pipeline:main"]   # module `pipeline`, function `main`
```

Crash it mid-run (`kill -9`), re-run the same command, and it resumes from where
it stopped. `keel flows` shows resumable/completed flows; `keel trace <flow>`
shows the step ledger.

### v0.1 limitations (precise, never silent)

Durability is a promise; a silent downgrade would be a Level 0 surprise. So this
is a hard, actionable error rather than a quiet fallback:

- **A journal is required.** Tier 2 replay lives in `.keel/journal.db`. If the
  native core can't attach one (e.g. `KEEL_JOURNAL=""` or an unwritable dir), a
  designated flow fails at startup with a config-level **KEEL-E005**
  (unsupported-configuration) naming the cause and fix — it does not run
  un-journaled. (The stub backend, which has no
  journal at all, reports the same class of error, pointing you at the native
  core.)

This is enforced before any effect fires.

### Async flow bodies

An `async def` flow entrypoint is supported: `keel run` drives it with
`asyncio.run`, and its `await`ed intercepted calls route through the **same**
open flow handle a synchronous flow's calls use — journaled and replayed
identically, with full Tier 1 resilience per step. Concurrent awaited effects
inside one flow (`asyncio.gather`) are admitted — and therefore journaled — in
the order their calls *reach* the flow handle, never in completion order, so
replay always reproduces the same step sequence. Keep fan-out order
deterministic (await sequentially, or fan out in a fixed, data-independent
order): if the runtime reaches the handle in a different order on resume
(racing tasks whose scheduling differs run-to-run), that is nondeterminism,
handled per `flows.on_nondeterminism` like any other divergence.

## Errors

Every Keel error carries a stable `KEEL-E0NN` code (see `keel explain <code>`),
a human first line (what / why / next), and — for machines — a `--json` twin on
the CLI. On terminal failure the original exception propagates unchanged, with a
`keel_outcome` attachment for those who look.

## Testing

```
python3 -m unittest discover python/keel     # front-end suite (stub; native legs run if keel_core is built)
```

Native-only tests (flows, persistent cache, native adapters) skip cleanly when
`keel_core` is not built.
