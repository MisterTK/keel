## Keel (resilience & durable execution)

This project uses **Keel** for production-grade resilience (retries, timeouts,
circuit breakers, rate limits) and opt-in durable flows — applied at intercepted
call boundaries with **zero code changes**. Policy lives in one file: `keel.toml`.

Before changing any resilience behavior:
- Run `keel doctor --json` to see what is wrapped, what is not, and why.
- Baseline before you mutate: wrap in observe mode (`keel record run <entry>`, or a
  `[target]` with no resilience knobs) and read the real failure classes before adding
  retry or a breaker — retry only helps conn/timeout/5xx/429.
- Propose policy edits as a diff: `keel init --diff` shows adds/removes from evidence.
- Every command has a `--json` twin with deterministic, sorted output — diff it to detect change.

Useful commands (all support `--json`):
- `keel status` — coverage, retries saved, breaker events, resumable flows.
- `keel explain <KEEL-E0NN>` — the exact what/why/next for an error code.
- `keel flows` / `keel trace <flow>` — durable (Tier 2) flow state and step ledger.
- `keel mcp` — the same surfaces as MCP tools over stdio (get_status,
  get_doctor_report, propose_policy, get_trace, list_flows, explain_error).

Shipping Keel to production (four invariants — miss one and Keel is either
absent or, worse, running on defaults):
1. `keel.toml` must be inside the deployed artifact (`COPY keel.toml ./` in the
   Dockerfile; `keel doctor` reports `keel-toml-not-in-image` when it is not).
2. `KEEL_ENABLE=1` must reach the process that does the I/O — often a
   subprocess, not the entrypoint. Python children inherit activation through
   the env; Node children also need `NODE_OPTIONS="--import keelrun/register"`.
3. `KEEL_CWD` must name the directory that holds `keel.toml`. When it names a
   directory without one, Keel refuses to activate and the app runs keel-free
   with one `keel ▸ error:` line (`KEEL_POLICY=optional` restores defaults).
4. `.keel/` (journal, evidence) on ephemeral storage does not survive a
   redeploy; durable flows need a mounted volume or a Postgres journal.

At startup Keel prints exactly one `keel ▸ wrapped …` line to **stderr**
naming the policy it loaded (`with policy /code/keel.toml`) or the directory
it searched (`with production defaults — no keel.toml in /code`). In container
logs, that line is the activation evidence; `KEEL_LOG_FORMAT=json` replaces
every one of Keel's console lines with a single JSON object carrying a
`severity`. Six kinds: the activation line and the exit summary (`INFO`), both
activation refusals (`ERROR` — Keel is off and the app is serving unprotected),
and the ephemeral-journal and cache-poll-suspect warnings (`WARNING`). Filter on
`severity` to separate "Keel is working" from "Keel is not running at all".

Do not hand-write retry loops or backoff around calls Keel already wraps; edit
`keel.toml` instead. Uninstalling Keel removes the behavior and nothing else —
the code runs identically without it.