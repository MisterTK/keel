# Keel v0.2.0 — agents are first-class

## Headline

Keel now wraps agent frameworks the same way it already wraps HTTP clients and
databases: ADK agent callbacks, tool calls, and MCP transports get retries,
circuit breakers, budgets, and crash recovery without touching agent code.
Two capabilities exist here that a generic resilience library can't offer —
durable multi-turn ADK agent runs that survive a process crash, and real
cross-model fallback on terminal model failure — plus a template, a Claude
Code Skill, and CI coverage across six real agent frameworks (ADK,
pydantic-ai, openai-agents, crewai, langgraph, MCP) backing the claim.

## What's new

Correctness (WS1). Two real bugs closed before anything else was built
on top of them: ADK callbacks and error handlers (`before_tool_callback`,
`on_tool_error`, etc.) now compose correctly with Keel's tool wrapping — a
rebind-on-first-sight redesign replaced an approach that could double-wrap
or skip callbacks depending on registration order. MCP tool-call failures
are now classified and counted correctly at the `tool:` layer (the
`{"error": "..."}` graceful-error-handling shape ADK returns is recognized
as a failure for breaker/discovery accounting, not silently treated as
success).

Activation (WS2). Zero-launcher activation: set `KEEL_ENABLE=1` and
Keel installs itself — no `keel run` wrapper needed. Python: the `keelrun`
wheel now ships an inert `.pth` file that arms the import hook when the env
var is set; `KEEL_CWD` relocates the config root for processes launched
from somewhere other than the project directory. Node:
`NODE_OPTIONS="--import keelrun/register"` is the equivalent gated entry
point. Fixes a real v0.1.1 bug found along the way: `keel run` had been
injecting `NODE_OPTIONS="--import keel/hook"` — the wrong module
specifier — instead of `keelrun/hook`; every `keel run` invocation against
a Node target was silently failing to preload the hook. Now correct.

Trust (WS3). CI's adapter farm now runs real legs against six pinned
agent-framework versions (`google-adk==2.4.0`, `pydantic-ai==2.9.0`,
`openai-agents==0.18.2`, `crewai==1.15.2`, `langgraph==1.0.10`,
`mcp==1.28.1`) plus a `uv`-installed wheel-activation leg — not fixtures
standing in for the real packages. Composition end-to-end tests prove
agent-before-callback execution and `on_tool_error` firing against the real
ADK `Runner`/`PluginManager`. `keel doctor` and `keel init` are
now agent-aware: the scanner recognizes `google.adk`, `google.genai`, and
the five framework packs; `keel init` redirects `keel.toml` into an
agents-cli project's agent directory (so it ships in the generated
Dockerfile's `COPY` set) and `keel doctor` warns when a root `keel.toml`
would otherwise be silently excluded from the container image.

Distribution (WS4). `packaging/agents-cli-template/` gives
`agents-cli scaffold create` a Keel-wrapped starting point out of the box.
A Claude Code Skill (`skills/keel/`) is installable via `npx skills add
MisterTK/keel` — the repository is public, so both this and the
`agents-cli create my-agent -a MisterTK/keel/packaging/agents-cli-template` template
consume path work today. A live demo (`demos/adk-demo`)
is certified against the real ADK stack. README and the `llms.txt`/`llms-full.txt`
surface now lead with the agent story.

Differentiators (WS5). Durable ADK Runner turns: designate
`py:google.adk.runners:Runner.run_async` in `[flows] entrypoints` and every
`Runner.run_async` call in the process becomes a Tier-2 durable flow — a
crashed or disconnected agent turn resumes from its last completed step on
re-invocation with the same `invocation_id`, instead of re-running LLM
calls and tool effects that already succeeded. Cross-model fallback: Keel's
ADK plugin implements `on_model_error` to chase a `fallback = [...]` chain
across model *providers* (not just same-host model-name rewrites) on
terminal model failure, budget- and breaker-aware, journaled like any other
effect.

## Known limitations

- One flow per process. A designated `Runner.run_async` call that lands
  while another Tier-2 flow is already open on the same backend does not
  queue or error — it proceeds unwrapped (noted once on stderr,
  `KEEL_QUIET`-aware). Nested/concurrent designated Runner flows in one
  process are not supported in v1.
- No time/random virtualization for Runner flows. Unlike `keel run`'s
  Tier 2, a designated `Runner.run_async` call does not journal/replay
  arbitrary `time.time()`/`random` reads inside the agent loop — only one
  correlation value (`adk:invocation_id`) is journaled. Nondeterminism
  inside a replayed agent turn (a different LLM sampling seed, wall-clock
  reads in tool code) is not defended against in v1.
- Cross-model fallback is non-streaming only. Streaming keeps the
  existing documented establishment-only stance.
- Pre-existing-resilience-library detection is Python-first.
  `keel doctor` detects co-occurring `tenacity`/`backoff`/`retrying`/
  `stamina` usage in Python projects; the equivalent Node-side detection is
  deferred (pre-existing gap, not new in this release; tracked as issue #21).
- `keelrun-cli-win32-x64` is still not live on npm. Blocked by npm's
  own anti-spam system, not a code or retry issue — tracked as issue #20,
  pending an npm support request (not yet filed) from the package owner;
  cargo/pip/uvx installs work on Windows today (pre-existing since v0.1.1;
  every other platform package and every other registry is unaffected).

## Upgrade notes

Nothing breaking. The `keelrun` Python wheel now ships a `.pth` file
alongside the package; it is inert unless `KEEL_ENABLE=1` is set in the
environment, so existing installs that don't opt into zero-launcher
activation see no behavior change.
