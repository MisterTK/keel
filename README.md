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
  timeouts, rate limits, caching, circuit breakers, and poll-until-terminal
  submit-then-poll loops are policy, not code — reviewed like
  infrastructure, not scattered through business logic.
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

Keel is published today — `pip`, `npm`, and `cargo` all work.

**Python** — library only, or library + the `keel` CLI in one line:

```bash
pip install keelrun                  # library only
keelrun-py-run your_app.py           # or: python -m keel run your_app.py

pip install keelrun keelrun-cli      # library + keel CLI (doctor/init/status/mcp/...)
keel run your_app.py
```

**Node** (≥ 22.5) — same shape:

```bash
npm install keelrun                  # library only
npx keelrun-node-run your_app.mjs

npm install keelrun keelrun-cli      # library + keel CLI
keel run your_app.mjs
```

**Rust** — the library (`#[keel::wrap]`) and the CLI are always separate
installs; `cargo add`/`cargo install` are different operations and cargo has
no single command spanning both:

```bash
cargo add keelrun --rename keel      # library — #[keel::wrap], see crates/keel/README.md
cargo install keelrun-cli            # CLI binary
```

**Just want the CLI, no persistent install, any language?**

```bash
uvx --from keelrun-cli keel run your_app.py
```

**Building from source** (contributors — needs Rust; `rustup` picks up the
pinned toolchain automatically):

```bash
maturin develop -m crates/keel-py/Cargo.toml     # builds the native core into your venv
pip install -e 'python/keel[dev]'
keel run your_app.py                             # or: python -m keel run your_app.py
```

Without the native module, the front end falls back to a pure-Python core:
Tier 1 resilience still works, but there's no persistent cache and no
durable flows.

## See it work

Five runnable, deterministic demos — no real network involved
([`tools/faultproxy`](tools/faultproxy) serves scripted faults). See the
40-second [storyboard](demos/STORYBOARD.md) for the shooting script.

| Demo | What it proves | Language |
|------|-----------------|----------|
| [flaky-python](demos/flaky-python) | A bare script dies on a 503; `keel run` survives it | Python |
| [node-service](demos/node-service) | Same story, Node: a bare script dies on a 500; `keel run` survives it | Node |
| [agent-demo](demos/agent-demo) | An LLM call survives a 429 storm; a second run costs ~0 API calls (dev cache) | Python |
| [adk-demo](demos/adk-demo) | A real `google-adk` agent's tool call survives a 429 storm below the agent loop — zero extra LLM tokens | Python |
| [durable-pipeline](demos/durable-pipeline) | `kill -9` mid-flow, rerun, and it resumes 10/10 steps — each firing exactly once | Python |

## How it works

Two tiers, one policy file:

- **Tier 1 — resilience.** Every intercepted call passes through a fixed
  layer chain: cache → rate limit → circuit breaker → timeout → retry.
  Stateless, works everywhere, needs nothing but the library.
- **Poll (submit-then-poll, opt-in).** A `poll` table turns a GET/HEAD call
  into a poll-until-terminal loop, judged on the response body instead of
  the transport result — the shape behind "submit a job, poll its status
  field until done":

  ```toml
  [target."api.example.com"]
  poll = { interval = "10s", deadline = "90s", until = { field = "status", terminal = ["completed", "failed"] } }
  ```

  A non-terminal `status` past `deadline` fails terminally with
  `KEEL-E016`; a response whose body isn't JSON (or lacks `until.field`)
  fails OPEN and is returned unchanged on the first attempt — polling never
  turns an ordinary response into an error.
- **Tier 2 — durable flows (opt-in).** Designate an entrypoint in `[flows]`
  and its steps are journaled to a local SQLite file (or Postgres, for
  fleet deployments) as they run. A crash — or a deliberate restart —
  replays completed steps from the journal instead of re-firing their side
  effects, then resumes live from wherever it left off.

### `keel exec` — durable external commands (CCR-4)

Wrap any command as a journaled durable flow — at-most-once dispatch per
identity, crash-safe retry gating, and a declared-side-effect gate:

    keel exec --flow autonomous-run \
      --journal-file logs/trades.jsonl \
      -- ./run_autonomous.sh

Honest scope: Keel gives you **at-most-once dispatch + crash-safe retry
gating**, not exactly-once execution inside an opaque child. A concurrent
same-identity invocation follows `[flows] on_busy = "skip" | "wait" |
"fail"` (default `skip` — the mkdir-mutex pattern this replaces). If a
failed run's declared journal files changed, a retry is refused with
KEEL-E033 (`--force` overrides). A completed flow re-invoked with the same
identity replays instantly without respawning the child.

#### In-process `cmd:` interception (`[flows.match]`, CCR-5)

The same `cmd:` guarantee is available with **no CLI wrapper**: declare an argv
match rule and Keel dispatches a matching subprocess call as a durable flow
from inside a live program.

    [flows]
    entrypoints = ["cmd:nightly-etl"]

    [flows.match."cmd:nightly-etl"]
    argv = ["./run_etl.sh", "*"]        # single-`*`, per-position, case-sensitive

When Keel is active in the process (`keel run`, or the `.pth`/`--import`
activation above), an observed argv matching the rule is wrapped instead of run
unwrapped — Python's `subprocess.run`/`check_output`/`call`/`check_call`, Node's
`spawnSync`/`execFileSync`. `on_busy` and the KEEL-E033 side-effect gate behave
as for `keel exec`. Shell-string commands are never matched (`shell=True`,
Node's `execSync`, `{ shell: true }`) — the shell, not the argv, decides what
runs.

The two front ends differ on **replay** today: **Python** gets full
replay-skip — a re-dispatched completed identity returns the recorded result
without respawning. **Node** gets at-most-once *dispatch* only; because its
`spawnSync`/`execFileSync` are synchronous they cannot reach the async replay
path (KEEL-E005), so a re-dispatch of a completed identity raises rather than
replays — `keel exec`/`keel flows` remain the replay workaround. Tracked in
[#42](https://github.com/MisterTK/keel/issues/42).

Both tiers run on the same native Rust core via a C ABI, so the Python and
Node front ends share identical semantics — verified by a shared
[conformance suite](conformance/README.md) that every implementation must
pass, not just documentation asserting it.

#### `keel flows force` — the durable escape hatch (CCR-6)

A failed `cmd:`/`keel exec` flow whose declared journal files changed on
disk refuses a retry with KEEL-E033, by design — Keel won't silently
re-run a side-effecting command against state it can no longer vouch for.
`keel flows force <flow-id>` is the deliberate, out-of-process override:
it durably marks that one flow-id as force-approved for its next retry, so
the KEEL-E033 gate steps aside exactly once. It is not a schema change or
a persistent policy flip — the approval is a single reserved marker-step
in the journal, consumed on the next dispatch, leaving no frozen-contract
footprint. Prefer `keel exec --force` when you're the one re-running the
command; reach for `keel flows force` when another process or operator
needs to clear the gate without re-invoking the original command itself.

## Agent integration

Two ways a coding agent picks up Keel:

- **A Claude Code Skill** (`packaging/claude-skill/keel/`) — covers adopting
  Keel in a project, day-to-day commands, and driving `keel mcp`. Install it
  by copying the directory into `~/.claude/skills/keel/` or a project's
  `.claude/skills/keel/`.
- **`keel init --agents`** drops a concise, deterministic section into
  `AGENTS.md` so every future agent session in an already-Keel-adopted repo
  inherits the ground rules without installing anything extra.

### Activation without `keel run`

When another tool owns the process launch (`agents-cli run`, `adk api_server`,
uvicorn, a test runner), Keel can activate as a plain dependency:

- **Python** — the `keelrun` wheel ships a site-packages `.pth` shim gated on
  one env var. Set `KEEL_ENABLE=1` (e.g. in your project `.env`) and every
  Python process in that environment boots with the same policy engine
  `keel run` uses — `keel.toml` from the working directory, or from
  `KEEL_CWD=<dir>` when your config lives in the deployable app directory.
- **Node** — add `NODE_OPTIONS="--import keelrun/register"` alongside
  `KEEL_ENABLE=1`.

Activation is fail-open by design: a broken install or invalid `keel.toml`
prints one `keel ▸` warning line and your app runs unwrapped. `KEEL_DISABLE=1`
always wins. The preflight resilience advisory stays a `keel run`-only,
CLI-side feature in both languages. Python's `.pth` shim additionally does
not wire `keel record`/`keel sim` or dispatch flow entrypoints — a `.pth` has
no target script to match `[flows] entrypoints` against. Node's
`keelrun/register` is a thin `KEEL_ENABLE` gate around the same preload
`keel run` uses — flow-entrypoint dispatch and
`KEEL_RECORD`/`KEEL_SIM_PLAN` wiring behave exactly as under `keel run`.

`keel mcp` serves the CLI itself as an MCP server over stdio — six tools,
each byte-identical to its `--json` CLI twin (`get_status`,
`get_doctor_report`, `propose_policy`, `get_trace`, `list_flows`,
`explain_error`). It has no `--project` flag; it always reports on its own
current working directory. Project-scoped `<project>/.mcp.json` (Claude
Code — already launched with the right `cwd`):

```json
{
  "mcpServers": {
    "keel": {
      "command": "keel",
      "args": ["mcp"]
    }
  }
}
```

Global config (Claude Desktop's `claude_desktop_config.json`, or any client
that doesn't launch from the project directory) needs an explicit `cwd`:

```json
{
  "mcpServers": {
    "keel": {
      "command": "keel",
      "args": ["mcp"],
      "cwd": "/absolute/path/to/the/project"
    }
  }
}
```

No `keel` on PATH (only installed via `uvx`)? Swap in
`"command": "uvx", "args": ["--from", "keelrun-cli", "keel", "mcp"]`.

### Keel for Google ADK + agents-cli

Three steps, no code changes:

1. **Dependency** — `uv add keelrun` (or start from the keel-enabled
   template: `agents-cli create my-agent -a MisterTK/keel/packaging/agents-cli-template`).
2. **Activate** — `KEEL_ENABLE=1` in your project `.env` (agents-cli
   propagates it to local runs, eval, and every deploy target).
3. **Policy** — `keel init` writes `keel.toml` into your agent directory
   (inside the Dockerfile's COPY set, so it ships in the container —
   `keel doctor` warns if it ever ends up at the repo root instead).

Every Gemini call (`llm:google-genai`), tool call (`tool:<name>`), and MCP
server round trip (`mcp:<server>`) becomes a policy-governed Keel target —
certified weekly against the real `google-adk` and `mcp` packages in CI,
including a full agent-over-MCP composition test. See `demos/adk-demo` for
a runnable 429-survival demo, and `skills/keel/` (`npx skills add
MisterTK/keel`) for the coding-agent skill.

Two ADK-specific capabilities beyond the target list above, both
farm-certified against the real `google-adk` package: list
`py:google.adk.runners:Runner.run_async` under `[flows] entrypoints` for
durable, crash-resumable agent turns (a designated `Runner.run_async` call
becomes a Tier 2 flow — see `docs/targeting.md` for the v1 limitations,
notably one flow per process); and set `fallback = [...]` on the
`llm:google-genai` target for cross-model fallback that survives a
provider switch, not just a same-provider retry — the plugin's
`on_model_error` hook resolves and constructs a real fallback model via
ADK's own `LLMRegistry`, the one seam that can build a request for a
genuinely different provider.

**`KeelSessionService`** is a journal-backed `google.adk.sessions.BaseSessionService`
(`keel.packs.adk_pack.KeelSessionService`) — ADK session state rides the
same Keel journal your flows already write, instead of a separate
in-memory or database-backed session store. Session writes
(`session_event`/`session_identity`/`session_delete`) are journaled as
steps through the currently-open Runner flow; reads reconstruct session
state from the journal via an in-process cache with a genuine cross-flow
journal-read fallback — never a second same-process SQLite connection
(that exact bug, [#14](https://github.com/MisterTK/keel/issues/14), is
the one thing this design goes out of its way to avoid on the read side
too). It only engages inside a designated `Runner.run_async` flow; outside
one it degrades silently to plain in-memory ADK session state, and if
you've designated the entrypoint but the call happens on the *wrong* flow
it raises KEEL-E005 loudly rather than writing to the wrong journal.
**Known limitation** ([#44](https://github.com/MisterTK/keel/issues/44)):
after a mid-turn crash and resume, the replayed session's substantive
content matches the pre-crash run exactly, but `Event.id`/`timestamp`/
`invocation_id` (and ADK-internal tool-call correlation ids) on the
replay-substituted prefix do not — ADK assigns those fresh on every run
and Keel does not virtualize them. If your integration depends on stable
event ids across a crash/resume boundary, this isn't there yet.

## Status

Keel is pre-1.0 and published on every registry (`pip`, `npm`, `cargo` — see
[Quickstart](#quickstart) above; the front-end name is `keelrun`, the CLI is
`keelrun-cli`, see `docs/naming-decision.md`). `brew install keel` is not
available — the Homebrew tap was deliberately not created (`cargo`/`pip`/
`npm`/`uvx` already cover every platform). Everything
described in this README is
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
