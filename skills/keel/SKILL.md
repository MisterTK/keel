---
name: keel
description: Use when adding production-grade resilience (retry/backoff/timeout/circuit-breaker/rate-limit/cache/poll-until-terminal) or opt-in durable, crash-resumable execution to a Python, Node/TypeScript, or Rust project; when evaluating, reviewing, or auditing whether and how Keel should cover a project, including a repo with no `keel.toml` yet; or when working in a repo that already uses Keel (a `keel.toml`, or an AGENTS.md "Keel" section, is present). Covers assessing fit, installing Keel, running `keel init`/`keel doctor`, wiring the `keel mcp` server, and reading `keel status`/`keel trace`. Invoke this before calling the `keel` MCP tools (`get_doctor_report`, `get_status`, `propose_policy`, `get_trace`, `list_flows`, `explain_error`) — they are diagnostic primitives this skill orchestrates. Do not use for building a workflow engine or queue system from scratch, for unsupported languages (only Python/Node/Rust), or for one-off retry logic in a codebase that has declined to adopt Keel.
---

# Keel

Keel is "the SQLite of durable execution": resilience (retry, backoff,
timeout, circuit breaker, rate limit, cache, poll-until-terminal) and
opt-in crash-resumable durable flows, applied at the call sites a target
project already makes — **zero code changes**. Policy lives in one file,
`keel.toml`. There is no service to run, no database to provision, and no
daemon.

## Is this project already using Keel?

Check for either signal before assuming a fresh install:
- A `keel.toml` at the project root.
- A `## Keel` section in `AGENTS.md` (written by `keel init --agents`).

If either is present, treat Keel as already adopted — go straight to
"Working in a Keel-adopted project" below. Do not re-run install steps or
suggest ad hoc retry code; both signals mean the ground rules there already
apply.

## Adding Keel to a project

Pick by language. The library (imported/depended on at runtime) and the
`keel` CLI (`run`/`doctor`/`init`/`status`/`mcp`/…, a devtool) are always
separate packages — install both together for the full experience, or the
library alone to stay lean:

```bash
# Python
pip install keelrun keelrun-cli          # library + CLI in one line
# or just the library:
pip install keelrun

# Node (>= 22.5)
npm install keelrun keelrun-cli          # library + CLI in one line
# or just the library:
npm install keelrun

# Rust — cargo has no single command spanning both operations
cargo add keelrun --rename keel          # library: #[keel::wrap]
cargo install keelrun-cli                # CLI binary

# Just want the CLI, no persistent install, any language:
uvx --from keelrun-cli keel run app.py
```

Then, from the project root:

```bash
keel init            # writes keel.toml from evidence: imports, call sites, observed traffic
keel doctor --json   # the honesty report — what's covered, what isn't, why
keel init --agents   # seeds the AGENTS.md section future agent sessions read
```

`keel init` never overwrites blindly — re-run `keel init --diff` any time to
preview what evidence would add or remove before touching the file.

**Known gaps, so as not to overpromise:** Rust has no `keel init --rust`
static-scan support yet (add the crate and call `keel::init()` yourself —
see `crates/keel/README.md` if working in this repo, or the published
crate's own README otherwise); a `cargo-keel` subcommand does not exist.

## Working in a Keel-adopted project

- Before changing anything resilience-related, run `keel doctor --json` to
  see what's wrapped, what's visible-but-unwrapped and why, and any findings
  (including, where built, a check for pre-existing retry/backoff code that
  might now be redundant with Keel's own).
- Never hand-write a retry loop, backoff decorator, or manual circuit
  breaker around a call Keel already wraps — edit `keel.toml` instead.
  `keel doctor` will flag known resilience libraries (e.g. `tenacity`,
  `backoff` on Python) still present so they don't silently compound.
- Propose policy changes as a diff, not a guess: `keel init --diff --json`
  shows exactly what evidence would add or remove.
- Every command has a deterministic `--json` twin (sorted keys, no
  timestamps) — diff two calls to see real change, don't parse prose.
- `keel explain <KEEL-E0NN>` gives the exact what/why/next for an error code
  without needing a web search.
- Uninstalling Keel (removing the package) restores the original behavior
  exactly — there is nothing else to revert.

## Evaluating Keel against a codebase (the protocol)

When asked whether/how Keel should cover a project — a fresh adoption or an
audit of an existing one — do NOT stop at grepping for HTTP libraries. Work
the five phases in order; the static scan is evidence, not the verdict.

1. **Scope.** Enumerate every process that does I/O, not just the entrypoint:
   the main app, MCP servers in `.mcp.json`, shell-script launchers, cron
   entries, anything reached via `subprocess`/`child_process`/`exec`. Keel
   must be installed *inside* a process to see its traffic — a sibling
   process is a coverage boundary, not a detail.
2. **Explore.** For each URL/host the code touches, trace how the request is
   *actually dispatched* — which library sends the bytes (an SDK may wrap a
   transport Keel adapts, or hide one it doesn't). Note stdlib transports:
   Python's `urllib.request` is adapted (wrapped like any registry library);
   `http.client` and raw `http`/`https` on Node are seen in the scan but not
   adapted yet.
3. **Collect.** Run `keel doctor --json` (or the `get_doctor_report` MCP
   tool). Read `topology` first — every sighted host lands in exactly one of
   `wrappable` ("wrap it"), `unreachable` ("can't reach it, here's why"), or
   `excluded` ("shouldn't reach it — seen only in a dependency-averse gate
   file; the exclusion is deliberate and overridable with `# keel: include`"),
   plus `external_processes` for the sibling-process blind spots. Then work
   `follow_ups` strictly top-down: it is ranked with rank 1 = the claim Keel
   is least able to verify itself (an unattributed URL) down to mechanical
   facts awaiting a decision. Codes are a closed set: `url-no-transport`,
   `subprocess-blind-spot`, `dependency-averse-excluded`,
   `preexisting-resilience`, `code-hash-stale`.
   Then read `findings` — it carries `warn` items that are not follow-up codes.
4. **Baseline before you mutate.** Before proposing any *behavior-changing*
   policy — retry, breaker, or a timeout that alters an outcome, as opposed to
   a pure simplification swap like a poll loop → `poll` policy — measure what
   actually fails. Wrap the candidate targets in observe mode (`keel record
   run <entry>`, or a `[target]` with no resilience knobs set — a no-knob
   wrap is pure passthrough plus events) and read the real failure-class
   distribution from `keel status --json` / the event sink. Retry only helps
   genuinely-transient classes (conn/timeout/5xx/429); an auth or validation
   4xx returns KEEL-E015 and is never retried, so wrapping it in retry buys
   latency, not resilience. Non-idempotent calls are `KEEL-E014` "observed,
   not retried" by default — confirm the transient hypothesis with evidence
   before recommending a behavior change.
5. **Analyze & propose.** Hunt hand-rolled resilience the scan may not flag
   yet: retry loops with sleeps, poll-until-status loops, `mkdir`-style
   mutexes, per-day guard files, broad `except: return None` swallows. Each
   is either replaced by policy (note which `keel.toml` key) or explicitly
   out of Keel's reach (say so honestly). Respect dependency-averse files —
   a stdlib-only gate/validator was built that way on purpose; never propose
   adding Keel as a dependency inside one. A shell-script orchestrator that
   builds its own at-most-once dispatch — `mkdir`/lockfile mutexes, guard
   files gating a retry, hand-rolled dead-PID checks around a launcher
   script — is out of the static scan's reach (it isn't Python/Node/Rust
   source) but is exactly what `keel exec --flow <name> [--journal-file
   <path>...] -- <command>` replaces: at-most-once dispatch per identity,
   crash-safe retry gating, and (with `--journal-file`) a declared-
   side-effect gate (KEEL-E033) before a failed run is retried. When the
   same subprocess call is launched *from inside* an already-Keel-active
   Python or Node process rather than a standalone shell script, prefer
   in-process `cmd:` interception instead — declare an argv match rule
   under `[flows.match."cmd:<name>"]` and Keel wraps the matching
   `subprocess`/`child_process` call directly, no `keel exec` wrapper
   needed; `keel doctor`'s `subprocess-blind-spot` follow-up now
   cross-references any `[flows.match]` rule that already covers a
   sighted call. A durable flow refused with KEEL-E033 can be cleared
   once, out-of-process, with `keel flows force <flow-id>` — a durable
   one-shot override, not a config change. Before
   resuming or trusting a durable flow's replay, check `code_hash_stale` in
   `keel flows --json` / `keel doctor --json` — `true` means the
   entrypoint's resolved code changed since the flow's last run, so a
   replay would substitute steps recorded against a different program;
   confirm the flow should still resume before doing so. Finish with
   `keel init --diff --json` / `propose_policy` and present the diff, never
   a hand-written policy guess.

## Driving Keel via MCP

`keel mcp` starts a stdio JSON-RPC MCP server exposing six tools, each
byte-identical to its CLI `--json` twin:

| Tool | CLI equivalent |
|---|---|
| `get_status` | `keel status --json` |
| `get_doctor_report` | `keel doctor --json` |
| `propose_policy` | `keel init --diff --json` (an applyable diff, never writes) |
| `get_trace` | `keel trace <flow> --json` |
| `list_flows` | `keel flows --json` |
| `explain_error` | `keel explain <code> --json` |

`get_doctor_report` includes `topology` (the three honesty buckets) and a
ranked `follow_ups` list — work follow-ups top-down; rank 1 means Keel is
least able to verify the claim itself.

**`keel mcp` has no `--project` flag** — it always reports on its own
current working directory, so whatever launches it must set `cwd` to the
target project's root, not wherever the client process happens to start
from. Two config shapes, depending on whether `keel` is already on PATH:

Project-scoped, `<project>/.mcp.json` (Claude Code — launched with `cwd`
already at the project root, so no explicit `cwd` needed):

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
that doesn't launch from the project directory) — `cwd` must be set
explicitly, or `keel mcp` reports on the wrong project or finds none at all:

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

If `keel` isn't installed globally (only via `uvx`), replace `command`/`args`
with `"command": "uvx", "args": ["--from", "keelrun-cli", "keel", "mcp"]` in
either shape above.
