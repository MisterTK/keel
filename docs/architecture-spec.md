# Keel — Architecture Specification
*An embedded, in-process durable execution and resilience runtime. Working title "Keel" (the part of the ship that keeps it upright without anyone thinking about it) — rename freely.*

**Version:** 0.1 draft · July 2026
**Owner:** TK
**Positioning in one line:** the SQLite of durable execution — resilience and durability as a library that lives *inside* the process, not a service the process talks to.

---

## 1. Requirements

### 1.1 Functional

- **FR1 — Auto-wrap.** Given an unmodified Python, TypeScript, or Rust project and a policy file, apply retries, exponential backoff with jitter, timeouts, response caching, rate limiting, circuit breaking, and idempotency-key injection to matching call sites without the user editing their functions.
- **FR2 — Policy as data.** All resilience behavior is declared in one file (`keel.toml`), scoped to targets: function paths, module globs, or outbound destinations (host/URL patterns). The same file is valid on a laptop and in production.
- **FR3 — Durable flows.** Designated entrypoints ("flows") journal every effect and survive process crashes: on restart, incomplete flows resume from the last completed step instead of rerunning side effects.
- **FR4 — Crash recovery with zero infrastructure.** Local state is a single journal file in the project directory. No daemon, no server, no external database, nothing to install beyond one binary and one language package.
- **FR5 — Observability for free.** Every wrapped call emits OpenTelemetry-compatible spans and metrics (attempts, backoff waits, cache hits, breaker state) without user instrumentation.
- **FR6 — Inspection and replay tooling.** `keel flows`, `keel trace <flow>`, `keel replay <flow>` for debugging; `keel doctor` scans a codebase and proposes policy targets.
- **FR7 — Simulation mode (later).** `keel sim` re-runs a workload with injected faults, latency, and crash-restarts to verify policies actually hold (deterministic simulation testing, laptop-sized).

### 1.2 Non-functional

- **NFR1 — No SDK leakage.** User code contains zero Keel types, imports, or context objects. The maximum permitted footprint is (a) entries in `keel.toml` and (b) in Rust only, an optional attribute macro (see §5.3 for why Rust is the exception).
- **NFR2 — Overhead.** Resilience-only interception ≤ ~10µs per wrapped call (excluding policy waits); journaled steps bounded by one local fsync (~0.1–1ms with SQLite WAL). Never on the hot path of unwrapped code.
- **NFR3 — One core, three languages.** A single Rust kernel with thin FFI bindings — never three reimplementations that drift.
- **NFR4 — Honest semantics.** The system never claims exactly-once side effects against external systems. It provides at-least-once execution with deduplication via idempotency keys, and says so.
- **NFR5 — Portability.** The CLI is a static Rust binary (musl) for macOS/Linux/Windows; the core builds for x86-64 and aarch64. No runtime dependencies.
- **NFR6 — Ownership.** Permissive or source-available license chosen by TK; no copyleft/BSL contamination from dependencies in the core path.

### 1.3 Constraints and assumptions

Solo developer initially, so the design must be shippable in vertical slices with value at every stage. Existing ecosystems (httpx, undici, reqwest…) must be met where they are — the runtime adapts to libraries, not vice versa. WASM componentization is explicitly *not* assumed for v1 (Python's ecosystem isn't there); it is reserved as a future "hermetic mode" (§8).

---

## 2. The central design decision: two-tier semantics

Auto-wrapping arbitrary, unmodified code cannot safely promise replay-based durability — arbitrary code is nondeterministic (threads, time, global state), and pretending otherwise produces silent corruption, the worst failure mode a durability product can have. But arbitrary code *can* safely receive retries, backoff, caching, rate limiting, circuit breaking, and observability, because those wrap individual calls without any determinism requirement.

Keel therefore has two explicit tiers:

**Tier 1 — Resilience mode (default, works on everything).** Policies applied at intercepted boundaries. No journal writes except metrics. No semantic constraints on user code whatsoever. This alone deletes the "150 lines of retry boilerplate" problem and is the v1 wedge.

**Tier 2 — Durable mode (opt-in per entrypoint, config-only).** A flow is designated *in the policy file* — e.g. `flows = ["pipeline.ingest:main", "jobs/*.run"]` — never by decorating code. Inside a flow, every intercepted effect becomes a journaled step; on crash, the flow re-executes and journaled results are substituted instead of re-firing effects. Determinism is required only *between* intercepted effects, and Keel actively detects violations (§4.4) rather than assuming compliance.

This split is what lets Keel be honest and zero-touch at the same time. It is the load-bearing idea of the design; everything else serves it.

---

## 3. High-level architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                        USER'S PROCESS                           │
│                                                                 │
│  user code (unmodified)                                         │
│      │  calls into stdlib / http libs / db drivers / functions  │
│      ▼                                                          │
│  ┌───────────────────────────────────────────────────────────┐  │
│  │ INTERCEPTION FRONT END (per language)                     │  │
│  │  py: import hooks + library adapters                      │  │
│  │  ts: ESM loader / --require + library adapters            │  │
│  │  rs: proc-macro + tower/reqwest middleware                │  │
│  └──────────────────────────┬────────────────────────────────┘  │
│                             │ FFI (PyO3 / napi-rs / native)     │
│  ┌──────────────────────────▼────────────────────────────────┐  │
│  │ KEEL-CORE (Rust kernel crate)                             │  │
│  │                                                           │  │
│  │  Policy Engine ──▶ per-target layer chain:                │  │
│  │   cache → rate-limit → breaker → timeout → retry →        │  │
│  │   idempotency → journal(step)                             │  │
│  │                                                           │  │
│  │  Flow Manager: step sequencing, replay, recovery, leases  │  │
│  │  Telemetry: OTel spans/metrics                            │  │
│  │  Journal trait ─┬─ sqlite file  (local, default)          │  │
│  │                 ├─ postgres     (team/enterprise)         │  │
│  │                 └─ object-log   (massive scale, later)    │  │
│  └───────────────────────────────────────────────────────────┘  │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
         ▲                                    │
         │ keel.toml (policy as data)         ▼ .keel/journal.db
   ┌───────────┐                       (single local file)
   │ keel CLI  │  run · flows · trace · replay · doctor · sim
   └───────────┘  (static Rust binary; embeds the same core)
```

Data flow, resilience mode: user call → front end intercepts → FFI → policy engine resolves the target's compiled layer chain → chain executes the underlying call with retry/backoff/etc. → result crosses back. Data flow, durable mode: identical, plus the journal layer records `(flow_id, step_seq, target, args_hash) → result` before the result is released; on recovery, the chain short-circuits to the journaled result.

The CLI and the language packages embed the **same core crate**, so behavior is identical whether code is launched via `keel run` or the language's native entrypoint with the Keel package present.

---

## 4. Deep dive: keel-core

### 4.1 Policy engine

`keel.toml` compiles at startup into per-target middleware chains, tower-style (ordered `Layer`s around a terminal `Service`). Example policy:

```toml
[defaults.outbound]                     # any intercepted network call
timeout   = "30s"
retry     = { schedule = "exp(200ms, x2, max 30s, jitter)", attempts = 5, on = ["5xx","timeout","conn"] }
breaker   = { window = "30s", failure_rate = 0.5, cooldown = "15s" }

[target."api.stripe.com"]
rate      = "90/s"
idempotency = { header = "Idempotency-Key" }

[target."GET api.catalog.internal/*"]
cache     = { ttl = "10m", scope = "persistent" }

[target."py:pipeline.enrich.*"]         # wrap these functions themselves
retry     = { schedule = "exp(1s, x2, max 5m)", attempts = 8 }
cache     = { ttl = "1h", key = "args" }

[flows]
entrypoints = ["py:pipeline.ingest:main", "ts:jobs/nightly.ts#run"]
on_nondeterminism = "fail"              # fail | warn | branch
```

Fixed layer order (cache outermost, journal innermost) keeps composition predictable; policies configure layers, never reorder them. The schedule grammar is the Effect-style algebra — schedules compose (`exp(...) upTo 10m andThen fixed(1m)`) — but lives in config, not code. Retry *conditions* are structured error classes (timeout, connection, HTTP status classes/exact codes) because the core sees typed errors from adapters, not language exceptions.

Rust building blocks: the layer chain, in-memory cache, token-bucket rate limiter, breaker, and schedule evaluator are all hand-rolled in `keel-core` directly on `tokio` (no `tower`/`moka`/`governor` dependency) — semantically owned, not outsourced.

### 4.2 Journal

A `Journal` trait with three planned backends; SQLite (default) and a real Postgres backend (`crates/keel-journal/src/postgres_journal.rs`) both ship — only the object-store-backed segmented log (§6, massive scale) remains unbuilt.

```rust
trait Journal: Send + Sync {
    fn begin_flow(&self, flow: FlowDescriptor) -> FlowId;
    fn record_step(&self, f: FlowId, seq: u64, key: StepKey, out: StepOutcome);
    fn lookup_step(&self, f: FlowId, seq: u64, key: &StepKey) -> Option<StepOutcome>;
    fn complete_flow(&self, f: FlowId, s: FlowStatus);
    fn incomplete_flows(&self, lease_expired: bool) -> Vec<FlowDescriptor>;
    fn acquire_lease(&self, f: FlowId, holder: ProcessId, ttl: Duration) -> bool;
    fn put_cache(&self, k: CacheKey, v: Bytes, ttl: Duration);
    fn get_cache(&self, k: &CacheKey) -> Option<Bytes>;
}
```

**SQLite over redb**, deliberately: WAL-mode SQLite gives crash-safe multi-*process* access on one machine (two terminals running scripts against one project journal — a real local scenario redb's single-writer model handles poorly), plus a universally inspectable file format (`keel trace` is a query; users can open the journal with anything). Cost: a C dependency in an otherwise pure-Rust core; acceptable, SQLite is the most battle-tested C on earth. Schema (abridged):

```sql
flows(flow_id, entrypoint, args_hash, code_hash, status, lease_holder,
      lease_expires, created_at, updated_at)
steps(flow_id, seq, step_key, kind, attempt, outcome, payload BLOB,
      error_class, started_at, ended_at, PRIMARY KEY(flow_id, seq))
cache(key, value BLOB, expires_at)
outbox(id, flow_id, destination, payload, status)      -- enterprise, later
```

Payloads are serialized with a versioned, self-describing format (MessagePack + schema tag). Default journal location `.keel/journal.db`; overridable to a shared path or a backend URL — that override is the *entire* laptop→enterprise migration for the user.

### 4.3 Flow manager: execution and recovery

A flow's identity is `(entrypoint, args_hash, explicit_key?)`. Steps are numbered by execution order; a step's key is `(target, args_hash)`. Recovery: on startup (and periodically), scan for incomplete flows with expired leases, acquire lease, re-execute the flow function from the top; each intercepted effect first consults the journal — hit means substitute recorded outcome without side effect, miss means execute live and record. This is the DBOS-style *coordinator-free* recovery model — no scheduler service exists, which is what keeps Tier 2 at zero infrastructure.

Leases (row-level, TTL + heartbeat) prevent two processes resuming the same flow. Retries *within* a step are the policy engine's business and are journaled as attempts of one step; re-execution *of the flow* is the flow manager's business. The distinction keeps "retry a flaky HTTP call" and "resume a crashed pipeline" from contaminating each other.

### 4.4 Nondeterminism defense

Replay correctness is checked, not assumed. During replay, the sequence of `(seq, step_key)` encountered must match the journal; a mismatch means the code path diverged (nondeterminism or a deploy changed the code). Response per policy: `fail` (default — halt with a precise diagnostic showing expected vs. actual step), `warn` (continue live from divergence point, journal a branch marker), `branch` (abandon replay, start a fresh attempt, keep the old record for audit). Additionally, `code_hash` per entrypoint fences deployments: replaying a flow recorded under a different code version downgrades to `warn`-at-best. Front ends also virtualize the cheap determinism leaks inside flows — `time.time()`, `Date.now()`, `random` — by journaling them as steps (they're effects too). This won't catch shared-memory races; the diagnostic-on-divergence is the honest backstop.

### 4.5 Telemetry

The core emits OTel spans (one per wrapped call, child spans per attempt) and metrics (attempt counts, backoff durations, cache hit ratio, breaker transitions, flow recovery counts) through `tracing` + `opentelemetry-rust`. Local default: pretty console summary + optional `keel trace` TUI reading the journal. Enterprise: standard OTLP export, configured in — where else — `keel.toml`.

---

## 5. Deep dive: interception front ends

The front ends are the product's UX; the core is its soul. Each front end translates "call site" into a core `TargetId` + typed request, and translates core outcomes back into idiomatic language results. All heavy logic stays in Rust.

### 5.1 Python (`keel-py`, PyO3)

`keel run app.py` (or `python -m keel app.py`) installs, before user code loads: (1) a `sys.meta_path` import hook that wraps functions matching `py:` targets at module import time — the wrapper is generated, user source untouched; (2) **library adapters** that patch known effect libraries at their narrowest stable seam: `httpx`/`requests`/`aiohttp`/`urllib3` (transport/adapter layer, so `host` targets work regardless of which HTTP sugar the app uses), `boto3` (botocore event hooks — official extension points), `psycopg` (connection factory). Async is bridged natively (PyO3 + pyo3-async runtime integration); sync calls run the core on a blocking path with GIL released during waits. Adapter fragility is managed with per-library-version contract tests in CI and a `keel doctor` warning when an unknown library version is detected — this is the maintenance tax of the no-code-changes promise, budgeted for, not wished away.

### 5.2 TypeScript/Node (`keel-node`, napi-rs)

Same shape: `keel run app.ts` injects `--import keel/hook` (ESM loader; `--require` for CJS). Adapters: global `fetch`/undici (dispatcher interception — one seam covers most modern HTTP), `pg`, `ioredis`, `mysql2`. Function-target wrapping happens in the loader by transforming module exports that match `ts:` targets. Bun/Deno support deferred; both have equivalent preload seams.

### 5.3 Rust (`keel` crate)

Rust has no runtime interception seam — no import hooks, no monkeypatching — so it is the honest exception to "zero code marks," held to a one-line ceiling: `#[keel::wrap]` on a function (proc macro reads the *same* `keel.toml` at compile time and generates the chain call), plus a `reqwest-middleware` client builder (`crates/keel/src/middleware.rs`) for outbound HTTP — the only drop-in middleware seam that actually ships. A `tower::Layer` for axum/tonic and a sqlx wrapper are not built, and `keel doctor`/`keel init` do not yet detect Rust projects at all (no `keel init --rust`); this is documented, tracked debt (see `crates/keel/src/lib.rs` module docs), not a silent gap. Rust developers tolerate an attribute; they riot at a framework. This satisfies NFR1's spirit: no Keel *semantics* in code — the attribute is a marker, all behavior stays in config.

### 5.4 What interception cannot see

Raw sockets, exotic native extensions, subprocesses (v1 treats a subprocess as one opaque step). Documented, surfaced by `keel doctor` ("these effects in your code are invisible to policies"), and the long-term answer is hermetic mode (§8).

---

## 6. Scale and reliability path

The user-visible contract: **the policy file and code never change; only `journal` changes.**

**Laptop (default):** `journal = "file:.keel/journal.db"`. Zero processes. Multi-process safe on one machine via WAL + leases. Recovery on next run (a crashed script resumes when re-invoked — correct semantics for CLIs and cron).

**Team / service (enterprise tier 1):** `journal = "postgres://…"`. Any replica of a service can recover any flow (leases arbitrate); fleet-wide flow visibility; the `outbox` table enables reliable event handoff. This covers the overwhelming majority of enterprise workloads and is DBOS-proven as an architecture — but implemented in *your* core with *your* semantics, which was the point.

**Massive scale (enterprise tier 2, later):** an object-store-backed segmented log with local write-ahead buffering, for workloads where Postgres write throughput on step records becomes the bottleneck; plus namespace/tenant partitioning. Design note: get the `Journal` trait boundary right in v1 (append-heavy, idempotent writes, no cross-flow transactions except leases) and this backend slots in without touching the engine. Load math for credibility: one journaled step ≈ 1 row insert ≈ hundreds of bytes; 10k flows/s × 20 steps ≈ 200k inserts/s — beyond single Postgres, hence the log backend exists; a laptop pipeline at 100 steps/s doesn't even wake SQLite up.

Failure modes and answers: process crash mid-step → step has no outcome, re-executes on resume (at-least-once), idempotency key makes the external call safe to repeat; journal corruption → SQLite WAL recovery, plus `keel fsck`; poison flows (fail on every resume) → attempt cap + `dead` status + `keel flows --dead`; clock skew on leases → TTLs generous (30s+) and monotonic-clock heartbeats.

---

## 7. Trade-off analysis

**Library-boundary interception vs. WASM host (the big one).** WASM gives total effect capture and enforced determinism but excludes most of real Python and imposes a compile step — it would make Keel *correct and unusable* in 2026. Library interception is partial and adapter-maintenance-heavy but works on every existing repo today. Chosen: interception now, WASM as an optional future mode, with the two-tier semantics (§2) sized so nothing promised exceeds what interception can deliver.

**Two-tier semantics vs. "durability everywhere."** Competitors market universal durability and bury the determinism caveats in docs; the resulting failure is silent wrong answers. Keel's split is more honest and produces a better funnel anyway (Tier 1 is adoptable in five minutes with zero risk). Cost: two mental models to document. Worth it.

**SQLite vs. redb:** decided in §4.2 for multi-process access + inspectability; revisit only if the C dependency blocks a target platform.

**One Rust core + FFI vs. native libraries per language:** FFI adds a boundary cost (~1–5µs) and async-bridging complexity, but three native implementations of retry/replay semantics *will* drift, and drift in a durability product is fatal. Core-plus-bindings is non-negotiable.

**At-least-once + idempotency vs. claiming exactly-once:** claiming exactly-once against arbitrary external APIs is false in every product that claims it. Keel journals *before releasing results* and injects idempotency keys, and documents the residual window. Honesty as differentiation.

**Build order risk:** the temptation is to build Tier 2 first because it's the interesting engineering. Resist: Tier 1's auto-wrap UX is the adoption wedge and de-risks the front-end fragility question — the thing most likely to kill the project — earliest.

## 8. Future: hermetic mode and simulation

Once WASI-0.3 toolchains mature for TS and Rust (and eventually Python), `keel run --hermetic` compiles/loads the target as a component under an embedded wasmtime host inside the same binary. All effects then flow through the host by construction: total capture, enforced determinism, and `keel sim` graduates from "fault-inject the adapters" (useful, ships earlier) to full deterministic simulation — record a workload, replay it under injected faults/latency/crashes, and *prove* the policy file holds. Same policy file, same journal, same CLI; hermetic mode is an upgrade, not a different product. This is also the moment the Golem/Obelisk comparison inverts: they are platforms you bring code to; Keel is a runtime that came to your code and can now optionally seal it.

## 9. Delivery roadmap (vertical slices, each independently valuable)

**M0 (weeks 1–4):** `keel-core` crate — policy engine, schedule algebra, SQLite journal, Rust API + `#[keel::wrap]`. Dogfood on your own Rust projects.
**M1 (weeks 5–10):** Python front end — `keel run`, import hook, httpx/requests adapters, Tier 1 only. *This is the public v0.1: "delete your tenacity boilerplate."*
**M2 (weeks 11–14):** Node front end, same scope. OTel export.
**M3 (weeks 15–22):** Tier 2 — flows, replay, recovery, nondeterminism defense, `keel flows/trace/replay`. v0.5: "your script survives ctrl-C."
**M4:** Postgres backend + leases across fleet. v1.0: the laptop→enterprise claim is real.
**M5:** adapter-level `keel sim`; begin hermetic mode spike.

## 10. Open questions

Serialization of language-native objects at step boundaries (start: JSON-compatible + bytes, fail loudly on unserializable, let adapters register codecs). Policy targeting syntax stability — worth a small spec doc of its own since it's user-facing API. Streaming responses through the cache/journal layers (chunked step outcomes vs. opt-out). Whether flow designation should also allow a code marker for users who *prefer* it (config remains the canonical path). License choice (Apache-2.0 for reach vs. BSL for defensibility) — decide before v0.1, repapering later is painful.

## 11. What I'd revisit as it grows

The fixed layer order (a plugin story will eventually demand extension points); MessagePack payload format (columnar/Arrow if analytics on journals becomes a feature); coordinator-free recovery (a tiny optional scheduler becomes attractive once cron-like flow triggering is requested — resist until pulled); the Rust attribute macro (if proc-macro-free build integration via `build.rs` + linker tricks proves viable, the last code mark disappears).
