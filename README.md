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
- **Observable when you need it, invisible when you don't.** Every run
  prints one summary to **stderr** at exit saying what Keel did — `keel ▸ 47
  calls · absorbed 3 rate limits · 2 retries succeeded · 4 calls
  unprotected` — and `keel report --open` turns the same evidence into a
  self-contained HTML page you can watch live with `--watch` or `--serve`.
  In a container this is the surface that survives; set
  `KEEL_LOG_FORMAT=json` to emit it (and the startup line) as one JSON
  object per line for Cloud Logging / CloudWatch. See
  [Observability](#observability) below. OpenTelemetry spans and metrics for
  every call and attempt are a source build with the `otel` feature plus one
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
uvx --from keelrun-cli keel report --open      # what did Keel do for this project?
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

`keel run` also wraps launch commands that aren't script files — console
scripts, `uv run`, `python -m`:

```bash
keel run -- uv run uvicorn app.fast_api_app:app --host 0.0.0.0 --port 8080
```

Keel execs the command with `KEEL_ENABLE=1` set, and every Python process in
the tree (the command itself, and any subprocess it spawns) self-activates
through the `keelrun` wheel — the same policy, journal, and discovery root
throughout. Requires `pip install keelrun` in that environment.

### First run → evidence → policy

Every run under `keel run` also records real traffic — calls, error rates,
latencies per target — into `.keel/discovery.db`. Use that before writing any
policy:

1. `keel run your_app.py` — zero config, and exercise the app a little (or
   run your test suite under it).
2. `keel init` — writes `keel.toml` from the observed evidence plus the
   static scan. Policy tuned to traffic you actually saw ("headroom over
   your observed mean") beats template guesses; with no observed runs the
   static scan alone still works, just with more conservative proposals.
3. `keel init --diff` — preview what any new evidence would change, any time.

## Observability

"What did Keel actually do?" has four answers, from zero setup to a live
dashboard — pick whichever fits the moment:

1. **Console summary — automatic, library-only.** Every process that runs
   under Keel (`keel run`, `python -m keel run`, the Node loader, `#[keel::wrap]`)
   prints one summary to **stderr** at exit, no CLI required:

   ```
   keel ▸ 185 calls · 1 failure not retried · 78 calls unprotected (storage.googleapis.com 41, metadata.google.internal 22, oauth2.googleapis.com 9, +2 others)
          keel report --open for the full picture
   ```

   A no-op run (nothing intercepted) stays silent. If the `keel` CLI isn't
   installed, that second line prints `uvx --from keelrun-cli keel report
   --open` instead — the summary itself never needs the CLI. Turn it off
   with `console = false` under `[telemetry]` in `keel.toml`, or `KEEL_QUIET=1`.

   `keel status` and `keel report` now show the **last activation**:
   language, version, the policy file it loaded (or `production defaults`
   and the directory it searched), and the pid — the local answer to "was
   Keel on, with which policy". `keel doctor --json` carries the same fact
   as `runtime_activation: verified | unverified`.

   Two runtime warnings name conditions the summary alone can't show: `keel
   ▸ warning: durable flows are configured but the journal is SQLite … on
   ephemeral storage` fires when `[flows]` is configured, the journal is
   SQLite, and a serverless marker, `/.dockerenv`, or a read-only cwd says
   this instance's filesystem won't survive a redeploy. `keel ▸ warning:
   <target> served 5 consecutive cache hits for one identical call over
   20s` fires when the default dev cache has been silently replaying what
   looks like a status-poll loop for at least 20 seconds.

   In a container, stderr is the surface that survives — though a parent
   that captures a child's stderr silently swallows it. Set
   `KEEL_LOG_FORMAT=json` and this line, the startup line, and any
   activation error each become one JSON object per line, so
   `policy_source`, `policy_path`, `keel_cwd` and `cache_hits` are queryable
   fields in Cloud Logging or CloudWatch rather than prose. The summary's
   `backend` field (Python only) names which backend actually ran —
   `"native"` or `"stub"` — since `KEEL_BACKEND=auto` silently falls back to
   the pure-Python backend when the native module can't be imported.

2. **Static HTML report — one command, no persistent install needed.**
   `keel report --open` (or, with no CLI installed at all, `uvx --from
   keelrun-cli keel report --open`) renders `.keel/report.html`: a single
   self-contained page — inlined CSS/JS, a strict CSP, zero external
   requests — with per-target call/retry/breaker/cache tables, a
   calls-vs-failures trend, the newest run's raw event stream, and durable
   flow status. It's just a file: screenshot it, attach it to a PR, email it.
   `keel report --json` prints the identical evidence as byte-deterministic
   JSON, for CI artifacts or feeding another tool.

3. **Live HTML report — two ways, both local-only, both CLI.**
   - `keel report --watch` rewrites `.keel/report.html` on an interval
     (default 2s) and the open page reloads itself — no networking, nothing
     to bind or firewall.
   - `keel report --serve` runs a loopback-only server (`127.0.0.1`, an
     ephemeral port by default) that the page polls once a second; a
     mismatched `Host` header is rejected (DNS-rebinding protection). Either
     way, Ctrl-C stops it cleanly.

4. **What this is not.** OpenTelemetry export (spans and metrics, not logs)
   requires a source build with the `otel` feature — the published wheels
   and npm addon do not include it — and lands in a tracing/metrics backend,
   never in your log search. Everything under `.keel/` is a local file: on a
   scale-to-zero platform it is gone by the time you investigate.

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
- **Poll (submit-then-poll, opt-in).** A `poll` table turns an idempotent
  status read into a poll-until-terminal loop, judged on the response body
  instead of the transport result — the shape behind "submit a job, poll its
  status until done". Terminals are matched by JSON type (`"done"`, `true`,
  `100`), and `until.field` may be a dotted path (`response.state`):

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

### Three clocks: `timeout`, `poll.deadline`, and the SDK's own

- **`timeout`** bounds **one attempt of one call**.
- **`poll.deadline`** bounds the **whole submit-then-poll loop** — the
  end-to-end budget an operator actually wants for a job or a render.
- **The SDK's client timeout** (google-genai, openai, plain httpx, often
  ~600s) is not reconfigured by Keel and fires first if it is shorter; Keel
  then just sees a retryable timeout. Set the SDK/call-site timeout to at
  least the Keel value for long calls.

`keel doctor` flags LRO-sized timeouts (>600s) with an `sdk-client-timeout`
follow-up. **POST-shaped operation reads poll too** (0.6.0): a `POST` to
`*.googleapis.com` ending in `:fetch*Operation` is judged idempotent — it is
retried under Level 0 defaults and gets the per-attempt `timeout` — identically
across all three front ends (Python, Node, and Rust) — and a
**route key** on the LLM host (a `[target]` key with a path) is consulted
before Keel's own host map, so the poll can carry its own policy:

```toml
[target."llm:google-genai"]
timeout = "120s"                       # chat and generate calls

[target."POST *-aiplatform.googleapis.com/*:fetchPredictOperation"]
timeout = "30s"                        # one poll attempt
poll = { interval = "10s", deadline = "30m", until = { field = "done", terminal = [true] } }
```

The route is an ordinary target (its own breaker, rate limit, and `keel
status` line, `defaults.outbound` underneath — no LLM budget, fallback, or
dev cache). A host-only glob such as `*.googleapis.com` never captures LLM
traffic. When a valid `keel.toml` is present, `keel doctor` attaches this
block as an applyable patch to the first `hand-rolled-poll` finding it can
attribute to an SDK poll call (one patch per provider; later findings for
the same provider point at it).

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

Both front ends get full **replay-skip**: a re-dispatched completed identity
returns the recorded result — Python's `CompletedProcess`/returncode, Node's
`spawnSync` result object or `execFileSync` stdout (re-throwing the recorded
error for a recorded nonzero exit) — without respawning the command. The one
case that refuses loudly in both is a recorded LAUNCH failure (the command
never ran, so there is nothing to substitute): change the argv/cwd for a fresh
identity, or re-drive it with `keel exec`. (Node reached parity in
[#42](https://github.com/MisterTK/keel/issues/42); before that it fenced the
re-dispatch with an error instead of replaying.)

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
  `keel run` uses — `keel.toml` from the working directory.
- **Node** — add `NODE_OPTIONS="--import keelrun/register"` alongside
  `KEEL_ENABLE=1`.

Both front ends read `keel.toml` from the working directory, and both honor
`KEEL_CWD=<dir>` to relocate that config root when your policy lives in the
deployable app directory rather than where the launcher happens to start the
process (`keel run` exports it to the children it spawns for exactly this
reason). When no `keel.toml` is found and one exists in a parent directory,
the startup banner says so and names the `KEEL_CWD` value that would load it.

`KEEL_CWD` is an assertion. If it names a directory with no `keel.toml`, Keel
prints `keel ▸ error: KEEL_CWD=… is set but …/keel.toml does not exist — Keel
NOT activated` and the app runs keel-free (`keel run` exits 2 instead).
`KEEL_POLICY=optional` restores the old behavior of running production
defaults with a warning. Without `KEEL_CWD`, a missing `keel.toml` still
means Level 0 defaults, and the banner names the directory it searched. In
Cloud Run / Lambda / Azure Functions (detected from `K_SERVICE`,
`AWS_LAMBDA_FUNCTION_NAME`, …) the default LLM dev cache is off unless
`KEEL_ENV=dev` says otherwise. This refusal *is* the fail-open behavior
described next — the `.pth`/preload can't halt the host process either way; it
just says so with an error line instead of a warning.

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

### Deploying with Keel

Keel is a file plus an env var, so reaching production has four invariants:

1. **`keel.toml` is in the artifact.** `COPY keel.toml ./` — `keel doctor`
   warns `keel-toml-not-in-image` when a root Dockerfile's `COPY`/`ADD`
   directives never reach it, and `keel init` prints the line to add.
2. **`KEEL_ENABLE=1` reaches the process doing the I/O.** Python children
   inherit it; Node children also need `NODE_OPTIONS`.
3. **`KEEL_CWD` names the policy's directory** from that process's point
   of view — point it at a directory with no `keel.toml` and Keel refuses to
   activate rather than run defaults (see above).
4. **`.keel/` outlives the instance** if you use durable flows — a volume
   or a Postgres journal. `keel doctor` warns `journal-ephemeral-storage`
   when `[flows]` is configured, the journal is SQLite, and the project
   root itself shows a deploy artifact (a build file, or
   `app.yaml`/`fly.toml`/`serverless.yaml`/`serverless.yml`) — a redeploy or
   scale-to-zero would otherwise discard every resumable flow silently.

Verify from the logs: exactly one `keel ▸ wrapped … with policy /code/keel.toml`
line per process, and — once the process has made at least one call, or at
exit — `keel doctor --json`'s `runtime_activation: "verified"`, which
requires an actual recorded activation whose policy matches this project,
not just a plausible-looking config. `keel init --agents` writes the same
four invariants into `AGENTS.md` so the agent that edits your Dockerfile has
them in context.

### Keel for Google ADK + agents-cli

Three steps, no code changes:

1. **Dependency** — `uv add keelrun` (or start from the keel-enabled
   template: `agents-cli scaffold create my-agent -a MisterTK/keel/packaging/agents-cli-template`).
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
