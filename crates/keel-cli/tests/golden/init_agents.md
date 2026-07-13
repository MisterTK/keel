<!-- keel:begin -->
## Keel (resilience & durable execution)

This project uses **Keel** for production-grade resilience (retries, timeouts,
circuit breakers, rate limits) and opt-in durable flows — applied at intercepted
call boundaries with **zero code changes**. Policy lives in one file: `keel.toml`.

Before changing any resilience behavior:
- Run `keel doctor --json` to see what is wrapped, what is not, and why.
- Propose policy edits as a diff: `keel init --diff` shows adds/removes from evidence.
- Every command has a `--json` twin with deterministic, sorted output — diff it to detect change.

Useful commands (all support `--json`):
- `keel status` — coverage, retries saved, breaker events, resumable flows.
- `keel explain <KEEL-E0NN>` — the exact what/why/next for an error code.
- `keel flows` / `keel trace <flow>` — durable (Tier 2) flow state and step ledger.
- `keel mcp` — the same surfaces as MCP tools over stdio (get_status,
  get_doctor_report, propose_policy, get_trace, list_flows, explain_error).

Do not hand-write retry loops or backoff around calls Keel already wraps; edit
`keel.toml` instead. Uninstalling Keel removes the behavior and nothing else —
the code runs identically without it.
<!-- keel:end -->
