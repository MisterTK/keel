# Keel — Weekend Subagent Sprint Plan
*Execution plan for building Keel with parallel teams of coding agents. Companion to the Architecture Spec and DX Spec — those documents are the source of truth; this one is the work breakdown.*

## Operating principles for agent-team development

**Contracts before code.** Parallel agent teams fail for one reason: integration surprise. So Sprint 0 produces frozen interface artifacts — actual files, not descriptions — and every team codes against those files. A team that wants to change a contract files a contract-change request to the orchestrator; nobody edits a contract unilaterally.

**Mock the core, build outward-in.** Front-end teams (Python, Node, CLI) build against `keel-core-stub`, a trivial in-memory implementation of the core FFI surface generated in Sprint 0. Integration day swaps the stub for the real core. This is what lets six teams run simultaneously instead of serially.

**Conformance suite as referee.** A shared test corpus (golden policy files, golden journal fixtures, a matrix of flaky-behavior scenarios) that every component must pass. "Done" means green on conformance, not "the agent says it works." Reviewer agents adversarially verify each team's PR against the spec sections they implement.

**Each team = builder + tester + reviewer.** The tester agent writes tests from the spec *before/parallel to* the builder writing code; the reviewer only reads spec + diff and tries to refute the claim of done. One orchestrator agent owns integration, contract changes, and the demo.

**Honest scope note:** the full architecture spec is not a weekend. The weekend target below is **v0.1 = Tier 1 end-to-end in Python + Node with LLM/agent-framework packs and the flagship CLI experience** — the adoption wedge. Tier 2 (durable flows) is scoped as a stretch track that ships only if conformance is green by Sunday noon; otherwise it's the following weekend's sprint. Cutting durability from the weekend is acceptable; cutting polish from Tier 1 is not (DX spec §1 Level 0 is the product).

---

## Sprint 0 — Friday evening: freeze the contracts (sequential, one team + orchestrator)

Deliverables (each is a file in `contracts/`):

1. **`policy.schema.json`** — the complete keel.toml JSON Schema, including target grammar (`host:`, `py:`, `ts:`, `rs:`, `llm:`, `tool:`, `mcp:`), schedule algebra grammar (EBNF), and the smart-defaults pack as a checked-in default policy document.
2. **`core-ffi.h` + `core_api.rs`** — the exact FFI surface: `keel_configure(policy_json)`, `keel_execute(target_id, request) -> outcome`, `keel_report(json_out)`, error taxonomy enum (`KEEL-E001…`), serialization envelope (MessagePack, versioned).
3. **`journal.sql`** — the SQLite schema, with golden fixture databases (a completed flow, an interrupted flow, a dead flow) for tooling teams to build against.
4. **`adapter-pack.md` + trait/interface stubs** — the `detect/seams/targets/defaults` contract from DX spec §4.3, in Rust trait + Python protocol + TS interface form.
5. **`keel-core-stub`** — in-memory fake core implementing the FFI surface (records calls, applies a trivial retry, returns canned outcomes). ~200 lines; unblocks four teams.
6. **`conformance/`** — scenario matrix v1: flaky HTTP (429/503/timeout/connreset sequences), rate-limit storms, breaker trip/recover, LLM 429 with Retry-After, cache hit/miss, non-idempotent POST must-not-retry. Runnable against stub from minute one.
7. **CI skeleton** — repo layout (cargo workspace + maturin + napi-rs), lint/test gates, contract-freeze check (CI fails if `contracts/` changes without an approved CCR label).

Exit gate: conformance suite runs green against the stub; orchestrator tags `contracts-v1`.

## Sprint 1 — Saturday: six parallel teams

**Team A — Core (Rust).** Architecture spec §4.1–4.2 Tier 1 scope: policy compiler (TOML → layer chains), schedule evaluator, retry/timeout/breaker/rate-limit/cache layers (tower + moka + governor), SQLite journal in *metrics/discovery mode only* (no replay yet), OTel span emission, FFI surface per contract. Acceptance: conformance green against real core in pure-Rust harness; overhead benchmark ≤10µs/call published as CI artifact.

**Team B — Python front end.** DX spec §1–2: `keel run` bootstrap, `sys.meta_path` hook, function-target wrapping, adapters for httpx + requests (aiohttp stretch), discovery recording, uninstall-clean behavior. Builds against stub. Acceptance: the flaky-script demo passes; `KEEL_DISABLE=1` yields byte-identical program behavior; conformance scenarios pass via Python harness.

**Team C — Node front end.** Mirror of B: ESM loader + `--require`, undici/fetch dispatcher interception, pg adapter (stretch), discovery. Acceptance: same bar as B on a Node demo app.

**Team D — CLI + auto-walk.** `keel run|init|doctor|status|explain` per DX spec §1–2, §6: static scanners (Python `ast`, TS via oxc), evidence-merged `keel init` generation with file:line comments, doctor honesty report, `--json` everywhere, deterministic output. Uses stub + golden journal fixtures. Acceptance: `keel init` on the three demo apps produces the golden policy files (snapshot-tested); every command has `--json` parity tests.

**Team E — LLM + agent-framework packs.** DX spec §4: `llm:` pack for openai/anthropic/google-genai SDK seams (Python + TS), provider-aware backoff honoring Retry-After, dev-mode LLM cache, model fallback chains; ADK plugin (Python) via its callback/plugin API; Vercel AI SDK middleware adapter; LangGraph node wrapping (checkpointer = stretch, post-weekend); eve/MCP: `mcp:` transport wrapping in the Node loader. Acceptance: an ADK demo agent and an AI-SDK demo survive injected 429-storms without agent-code changes; dev cache demonstrably cuts a repeated agent run's API calls to ~0.
*Dependency note: E consumes B/C's interception seams — E starts on pack `detect()/defaults()` + provider seam research in the morning and lands integration after B/C's midday checkpoint.*

**Team F — Conformance, demos, docs.** Expands the scenario matrix (fault-injection proxy binary all teams reuse), builds the four demo apps (flaky Python script, Node service, ADK agent, LangGraph pipeline), writes README + llms.txt + `keel explain` error-code corpus from the frozen taxonomy, records the 40-second asciinema storyboard. Acceptance: demos run scripted end-to-end against the stub Saturday night, ready to re-run against real core Sunday.

Saturday midday sync (orchestrator): B/C seams stabilize → E integrates; any contract friction resolved by CCR. Saturday night: all teams green against stub or real core in isolation.

## Sprint 2 — Sunday: integration and the wedge

**Morning — the swap.** Replace stub with real core behind B, C, D, E (maturin + napi builds from CI). Run full conformance in every language harness. Expect the day's bugs at exactly three seams: FFI serialization, async bridging (PyO3-async / napi tokio), and error-type mapping — the orchestrator triages nothing else until these are green.

**Midday gate — Tier 2 go/no-go.** If conformance is green by ~noon: Team A + a fused B/D squad take the stretch track — minimal durable flows (flow designation from config, step journaling, resume-on-rerun for the Python demo only, `fail`-mode nondeterminism response). Everyone else moves to polish. If not green: all hands on Tier 1 polish; durability moves to next sprint, and v0.1 loses nothing that its story needs.

**Afternoon — polish is the feature.** Error-message copy review against DX invariant #4 (every message: what/why/next/trace-ref). `keel status` screen. Startup-time budget enforcement (<100ms). `keel doctor` on all four demos reads clean. README + asciinema recorded for real.

**Evening — release candidate.** Tag v0.1.0-rc1: wheels (macOS/Linux, x86-64/aarch64), npm package, cargo crate, brew formula draft. The orchestrator runs the demo script cold on a clean machine (agent in a fresh container): `uvx keel run` on a never-seen flaky script must just work. That cold-run is the release gate — not the test suite.

## Standing structure after the weekend

Subsequent weekend sprints, in order of leverage: (1) Tier 2 durable flows complete + `keel flows` UX; (2) adapter-pack expansion sprint — the §4.3 contract makes each framework (Pydantic AI, OpenAI Agents SDK, CrewAI, eve deep-integration, LangGraph checkpointer) a one-team task; (3) `keel mcp` server + agent-docs surface (DX §5); (4) Postgres journal + fleet recovery; (5) `keel record test` + sim mode.

(Status update, 2026-07-13: all five items above have since shipped, via the follow-on multi-wave completion program on branch claude/complete-keel merged into main as PR #6 — see CLAUDE.md.)

Permanent roles: contract steward (guards `contracts/` — the compatibility promises), adapter CI farm (contract tests against pinned framework/library versions, the maintenance tax made visible), and a weekly "cold-machine run" as the recurring quality ritual.

## Risk register (weekend-specific)

Top three ways this weekend fails, and the countermeasure baked in: **(1) contract churn mid-Saturday** → CCR-only changes, orchestrator arbitration, stub insulates teams; **(2) async FFI bridging eats Sunday** → Team A builds the PyO3-async/napi-tokio bridge *Saturday* as part of the FFI surface with a dedicated bridge conformance test, not discovered Sunday; **(3) scope creep toward Tier 2 glamour** → the midday gate is binary and the orchestrator enforces it; the DX spec's Level 0 polish list is the pre-agreed definition of "what we polish instead."

---

## Completion annotation (2026-07-12)

The weekend's plan was executed as 17 tasks; all landed. What shipped against the plan:

- **Sprint 0 (contracts + stubs + conformance):** shipped as planned — `contracts-v1` frozen, three-language stubs, conformance corpus (grown from 15 to **17** scenarios with the two Tier 2 additions) (superseded: the follow-on wave program grew the corpus further; conformance/scenarios/ currently holds 34 scenario files spanning numbers 01-27 — see conformance/README.md).
- **Sprint 1 (the real core + both front ends + CLI):** shipped. `crates/keel-core` (engine + `keel-journal` discovery/flow journal + OTel), `crates/keel-ffi` (C ABI + MessagePack) with **PyO3 and napi async bridges** (risk #2 retired with a bridge conformance test, as planned). Python + Node front ends with httpx/requests/fetch adapters and `llm:`/`mcp:`/AI-SDK packs. CLI `run·init·doctor·status·explain` — plus `flows·trace` (below).
- **Sprint 2 midday gate — Tier 2: GO.** Durable flows shipped beyond the "Python demo only" floor: flow journaling, resume-on-rerun with step substitution, `fail`/`warn`/`branch` nondeterminism responses, leases + attempt cap, and time/random virtualization, all in the real core; the Python front end designates flows from `[flows]`; `keel flows`/`keel trace` expose them. The `kill -9` → resume demo is a real subprocess test.
- **Overhead:** measured ~0.56 µs worst-case wrapped call at weekend's end (2026-07-12); after the follow-on wave program added the event sink and idempotency-key injection, the current measured worst case is ~0.8 µs (target/bench-overhead.json), still well inside the 10 µs budget (DX invariant 8), emitted as a CI artifact.

Delivered beyond / deferred from the RC-1 evening list:
- **Delivered:** four runnable demos + `tools/faultproxy` (deterministic, no network), the 40-second storyboard as a script, `llms.txt`/`llms-full.txt`, and `keel init --agents`.
- **Deferred (honest gaps):** wheels/npm/brew are **not published** — everything builds from source; the asciinema is a shooting script, not a recording; Postgres/fleet journal (Level 3), `keel mcp`, `keel record test`, and further adapter packs (LLM + agent-framework: pydantic-ai, openai-agents, crewai, ADK, LangGraph checkpointer, AI-SDK) were deferred as of this 2026-07-12 annotation but have since shipped in the follow-on multi-wave completion program (branch `claude/complete-keel`, merged into `main` as PR #6) — see CLAUDE.md for current status. Still not done: package-registry publishing (wheels/npm/brew) and a recorded (vs. scripted) asciinema.

The referee held throughout: conformance green in every harness at each step, `contracts/` unchanged since freeze.
