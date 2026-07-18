# Keel demos

Five runnable demos, each `./run.sh` self-contained and **deterministic** (no
real network — [`tools/faultproxy`](../tools/faultproxy) serves scripted fault
sequences). They prefer the repo's `.venv` (which has the native core); set
`KEEL_PYTHON=/path/to/python` to override.

| Demo | What it proves | Language | Needs native core |
|------|----------------|----------|-------------------|
| [`flaky-python`](flaky-python) | Bare httpx script dies on a 503; `keel run` retries it and it survives — zero code changes | Python | no |
| [`node-service`](node-service) | Bare `fetch` dies on a 500; `keel run` retries it | Node ≥22.5 | no |
| [`agent-demo`](agent-demo) | Fake LLM endpoint: rides a 429 storm, then dev-cache replays so a 2nd run makes ~0 API calls | Python | for cross-run replay |
| [`adk-demo`](adk-demo) | A real `google-adk` `LlmAgent`'s tool call rides out a 429 storm BELOW the agent loop — one agent turn, zero extra LLM tokens | Python (needs `google-adk`) | no |
| [`durable-pipeline`](durable-pipeline) | 10-step flow `kill -9`'d mid-run resumes from the journal; each step runs exactly once | Python | yes (Tier 2) |

[`STORYBOARD.md`](STORYBOARD.md) is the 40-second asciinema shooting script
(dx-spec §6) — the README hero demo, backed by `flaky-python` + `durable-pipeline`.

## Smoke coverage

Each demo is executed by a test (not just documented):

- `flaky-python`, `agent-demo`, `adk-demo` → `python/keel/tests/test_demos.py`
- `node-service` → `node/keel/test/demo.e2e.test.mjs`
- `durable-pipeline` → `python/keel/tests/test_resume_demo.py` (the real
  `kill -9` + resume assertion)

faultproxy itself is unit-tested in `tools/faultproxy/test_faultproxy.py`.

## Framework packs: adk-demo

`agent-demo` above is **not** an ADK agent — it models an LLM completion as a
bare intercepted HTTP call (`demos/agent-demo/agent.py` is a plain
`httpx.get`). [`adk-demo`](adk-demo) is the real thing: a genuine
`google.adk` `LlmAgent` with one `FunctionTool` (`fetch_answer`) whose body
makes an ordinary `httpx.get` to a faultproxy endpoint serving `429, 429, 200`
(`agent-demo`'s scenario, reused), driven by a scripted `BaseLlm` (no live
LLM, no network beyond loopback — see `demos/adk-demo/agent.py`'s
`ScriptedModel`). Keel's retry rides out the storm *inside* the tool call,
below the agent loop: 3 upstream calls happen, but the scripted model only
ever takes one function-call turn and one final-text turn — zero extra LLM
tokens burned. `agent.py` (and `run.sh`) skip cleanly with a
`pip install google-adk` hint when `google-adk` isn't importable, so this
demo never breaks a stub-only checkout. The Google ADK pack itself
(`python/keel/src/keel/packs/adk_pack.py`, tool-rebind + plugin
auto-registration) isn't exercised by this demo — the whole point is that the
retry needs no ADK-specific integration at all, just the same intercepted
`httpx.get` seam `agent-demo` already uses. `adk_pack` has its own coverage:
a structural fake (`python/keel/tests/test_packs_adk.py`) plus a real-library
farm leg (`python/keel/tests/test_farm_adk.py`,
`test_farm_adk_composition.py`).
