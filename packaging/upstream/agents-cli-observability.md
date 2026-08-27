# Upstream contribution: add Keel to agents-cli's observability skill

> **STALE as of 2026-08-27 — do not send as-is.** Re-checked against a local
> `google/agents-cli` checkout at v1.4.1 (`/Users/tk/dev/agents-cli`) during
> the agents-cli certification pass. Both target tables in this skill have
> moved since this doc was written, and neither is actually the right place
> for Keel anymore:
>
> 1. **"Third-Party Integrations" table** (`SKILL.md` ~line 122, current
>    v1.4.1) is now explicitly sourced from an EXTERNAL, Google-curated page
>    — `https://adk.dev/integrations/` — the skill's own local table is only
>    a 7-row excerpt of it (AgentOps, Arize AX, Phoenix, MLflow, Monocle,
>    Weave, Freeplay), all genuine **tracing/observability SaaS or
>    self-hosted platforms**. A PR adding a row here, in `agents-cli`'s repo,
>    would be editing the wrong copy even if accepted — the authoritative
>    list lives elsewhere. It's also a weak category fit: Keel is a
>    resilience library with an NDJSON side-channel, not a tracing/eval
>    platform in the same class as the existing rows.
> 2. **"Deep Dive: ADK Docs (WebFetch URLs)" table** (`SKILL.md` ~line 148,
>    current v1.4.1) has NARROWED since this doc was written — every row is
>    now an `adk.dev`-hosted doc page (observability overview, logging,
>    Cloud Trace, BigQuery Agent Analytics). It is no longer a general
>    "external deep dive" table; a `github.com/MisterTK/keel` row would be
>    the only non-`adk.dev` entry and doesn't fit the table's current scope.
>
> **Real next step**: find whatever repo hosts `adk.dev/integrations/`'s
> content (not `google/agents-cli`, not `google/adk-python` — checked both
> locally, neither contains it) and follow ITS submission process instead.
> That is a fresh investigation, not something this doc's content can be
> resubmitted into. The historical draft below is kept for its verified
> claims about Keel's own observability surface (event stream, CLI
> introspection, OTel export) — reusable prose for whatever the real
> submission turns out to need — but the two insertion points and the PR
> body's table-row framing are obsolete.
>
> Separately, the SAME certification pass found a genuinely current,
> correctly-targeted gap in the **scaffold** skill (not observability):
> `google-agents-cli-scaffold`'s "adk is the only template" framing gives no
> indication that `--agent`/`-a` also accepts a remote Git spec for
> layering a vendor add-on (the exact mechanism Keel's own template uses) —
> drafted and staged as a real fix, see
> `add-keel-scaffold-addon-discoverability-2026-08` branch in the local
> `/Users/tk/dev/agents-cli` checkout (2 files, 1 commit, not pushed).

Target: `google/agents-cli`, file
`skills/google-agents-cli-observability/SKILL.md`.

This document is paste-ready. TK opens the PR under their own name — the PR
body below intentionally has **no Claude/AI attribution footer**; it is
TK's contribution to send, not a Claude Code artifact.

---

## Preconditions before sending

**The Keel repo (MisterTK/keel) must be made public before sending this PR.**

**SATISFIED as of 2026-07-16.** The repo was flipped to public and the Deep
Dive URL (`https://github.com/MisterTK/keel/blob/main/llms.txt`) was
re-verified to resolve unauthenticated (`curl` returns 200) on that date —
it no longer 404s for skill reviewers or the skill's WebFetch consumption.
This precondition is kept below as the record of what was checked and why;
the note it protects against — a private repo 404ing the Deep Dive link —
no longer applies. If the GitHub org or username ever changes, update both
the platform-table row and Deep Dive row accordingly, and re-verify the URL
again before sending.

---

## 1. Platform table row

Insert as a new row in the "Third-Party Integrations" table
(`SKILL.md` lines ~111-119), keeping the existing column order and the
bold-platform-name convention:

```markdown
| **Keel** | Zero-code resilience policy, deterministic replay, crash-resumable flows | Minimal | Yes (local files) |
```

## 2. Deep Dive row

Insert as a new row in the "Deep Dive: ADK Docs (WebFetch URLs)" table
(`SKILL.md` lines ~139-151), in the backticked-URL style the surrounding
rows use:

```markdown
| Keel | `https://github.com/MisterTK/keel/blob/main/llms.txt` |
```

(`MisterTK/keel` is the actual repo — must be made public before sending.)

## 3. PR body

```
## Summary

Adds Keel to the observability skill's third-party integrations table.
Keel is a policy-driven resilience layer (retry, backoff, timeout,
circuit breaker, rate limiting, caching, and crash-resumable durable
flows) that attaches to a Python, Node, or Rust process with zero code
changes — a `keel.toml` policy file plus either the `keel run` wrapper
or a one-line `.env` opt-in (`KEEL_ENABLE=1`) for the wheel/package's
auto-activation shim. For agents-cli/ADK projects specifically, that
`.env` opt-in fits naturally alongside the project's existing
`load_dotenv()` call, so enabling Keel requires no code changes to the
agent.

On the observability side, Keel exposes:
- **Structured event stream**: every attempt, retry, cache hit/miss,
  breaker transition, and flow resume is written as an NDJSON line to a
  local event log — nothing proprietary, just files you can tail or ship
  wherever you already collect logs.
- **CLI introspection**: `keel status`, `keel trace <flow>`, and
  `keel tail` read that same event log to answer "what happened" and
  "what's in flight" without a dashboard or SaaS account.
- **Deterministic `--json` output and an MCP server**: every CLI command
  has a `--json` twin with sorted keys and no timestamps, and `keel mcp`
  exposes the same data (`get_status`, `get_trace`, `get_doctor_report`,
  `list_flows`, `propose_policy`, `explain_error`) as MCP tools whose text output is
  byte-identical to the matching `--json` command — so a coding agent or
  a CI check can diff two calls and see only real change.
- **Optional OTel export**: spans and metrics (attempts, retries,
  backoff waits, cache hit ratio, rate-limit throttling, breaker
  transitions, flow resumes) are available behind the `otel` Cargo
  feature and the `KEEL_OTEL` runtime gate. This is additive, not a
  replacement: it composes with ADK's own OpenTelemetry instrumentation
  (Cloud Trace, or any OTLP collector) rather than displacing it, so
  Keel's spans/metrics show up as more detail in the same trace
  pipeline rather than a second, competing one.

Keel is resilience-first, not a tracing platform: it does not do session
replays, hosted dashboards, or eval tooling the way AgentOps/Arize
AX/Phoenix/etc. do (see the row's differentiator text, which deliberately
avoids those claims). It sits in the table because its NDJSON stream and
local-first `status`/`trace`/`tail` surface are a legitimate lightweight,
self-hosted option next to those platforms, and its OTel output is one
more thing users can feed into whichever of those platforms they pick.

## Test plan

- [ ] Table renders correctly and column semantics match neighboring
      rows (spot-checked above)
- [ ] Deep Dive link resolves (`https://github.com/MisterTK/keel/blob/main/llms.txt`)
```

---

<!--
Verification notes for TK's reviewer — claims checked against
MisterTK/keel @ worktree-agent-first-class before this doc was written:

- otel feature gating (spans + metrics behind the `otel` Cargo feature,
  no OpenTelemetry dependency without it, `KEEL_OTEL` runtime gate):
  crates/keel-core/src/otel.rs (module doc + `GATE_VAR = "KEEL_OTEL"`),
  crates/keel-core/src/metrics.rs (module doc: "Without the `otel` cargo
  feature every function here is an empty no-op and the module pulls
  zero dependencies").
- OTel instrument set (attempts, retries, backoff waits, cache hit
  ratio, rate-limit throttling, breaker transitions, flow resumes):
  crates/keel-core/src/metrics.rs (module-doc instrument table).
- `keel status` / `keel trace` / `keel tail` exist as CLI subcommands:
  crates/keel-cli/src/main.rs (subcommand dispatch, `Command::Trace`
  variant lines 136–139), crates/keel-cli/src/status.rs, crates/keel-cli/src/tail.rs.
- NDJSON event sink: crates/keel-core/src/events.rs (`EventKind`,
  `EventSink`, module doc: "The event vocabulary, tagged `"event"` with
  `snake_case` names").
- Deterministic `--json` / MCP surface, byte-identical to CLI JSON:
  crates/keel-cli/src/mcp.rs (module doc: "each is a thin wrapper over
  the same library producer as the corresponding CLI command, so a
  tool's text result is byte-identical to that command's `--json`
  output (golden-tested)"; `INSTRUCTIONS` const spells out the
  `get_status`/`get_trace`/etc. mapping), golden-tested by
  crates/keel-cli/tests/mcp.rs
  (`mcp_tool_outputs_are_byte_identical_to_the_json_twins`).
- `.env`-only activation (`KEEL_ENABLE`, zero code changes):
  python/keel/keelrun_activate.pth (site-packages `.pth` gate on
  `KEEL_ENABLE`), python/keel/src/keel/_auto.py (auto-activation shim
  doc + `_TRUTHY` check). Confirmed agents-cli scaffolds already call
  `load_dotenv()` (e.g.
  src/google/agents/cli/scaffold/deployment_targets/cloud_run/python/{{cookiecutter.agent_directory}}/fast_api_app.py
  in the google/agents-cli repo), so a `.env`-set `KEEL_ENABLE=1` needs
  no code change in an agents-cli project.
- Crash-resumable flows (journal-backed resume, at-least-once honesty
  rule, code_hash divergence handling): crates/keel-cli/src/resume.rs
  (`resumable_candidates`, resumability doc comment), conformance
  scenarios conformance/scenarios/24-flow-crash-mid-step-reexecution.json
  and conformance/scenarios/16-flow-resume-substitutes-steps.json, unit
  test crates/keel-core/tests/flows.rs
  (`crash_after_step_three_resumes_substituting_completed_steps`).
- Repo URL: `MisterTK/keel` as confirmed by `git remote -v` →
  `git@github.com:MisterTK/keel.git`; `Cargo.toml` `repository =
  "https://github.com/MisterTK/keel"`; `llms.txt` exists at repo root.
  Repo is public (verified 2026-07-16, `gh repo view` →
  `isPrivate: false`); Deep Dive link resolves unauthenticated — no longer
  a blocker for PR merge.
- Upstream table/row formats matched against
  /Users/tk/dev/agents-cli/skills/google-agents-cli-observability/SKILL.md
  (Third-Party Integrations table, lines ~111-119; Deep Dive table,
  lines ~139-151) as of this writing.
-->
