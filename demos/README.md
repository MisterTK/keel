# Keel demos

Four runnable demos, each `./run.sh` self-contained and **deterministic** (no
real network — [`tools/faultproxy`](../tools/faultproxy) serves scripted fault
sequences). They prefer the repo's `.venv` (which has the native core); set
`KEEL_PYTHON=/path/to/python` to override.

| Demo | What it proves | Language | Needs native core |
|------|----------------|----------|-------------------|
| [`flaky-python`](flaky-python) | Bare httpx script dies on a 503; `keel run` retries it and it survives — zero code changes | Python | no |
| [`node-service`](node-service) | Bare `fetch` dies on a 500; `keel run` retries it | Node ≥22.5 | no |
| [`agent-demo`](agent-demo) | Fake LLM endpoint: rides a 429 storm, then dev-cache replays so a 2nd run makes ~0 API calls | Python | for cross-run replay |
| [`durable-pipeline`](durable-pipeline) | 10-step flow `kill -9`'d mid-run resumes from the journal; each step runs exactly once | Python | yes (Tier 2) |

[`STORYBOARD.md`](STORYBOARD.md) is the 40-second asciinema shooting script
(dx-spec §6) — the README hero demo, backed by `flaky-python` + `durable-pipeline`.

## Smoke coverage

Each demo is executed by a test (not just documented):

- `flaky-python`, `agent-demo` → `python/keel/tests/test_demos.py`
- `node-service` → `node/keel/test/demo.e2e.test.mjs`
- `durable-pipeline` → `python/keel/tests/test_resume_demo.py` (the real
  `kill -9` + resume assertion)

faultproxy itself is unit-tested in `tools/faultproxy/test_faultproxy.py`.

## Framework packs: no ADK demo yet (noted, not built)

`agent-demo` above is **not** an ADK agent — it models an LLM completion as a
bare intercepted HTTP call (`demos/agent-demo/agent.py:18-21` is a plain
`httpx.get`). The Google ADK pack itself
(`python/keel/src/keel/packs/adk_pack.py`) is implemented and tested against a
structural fake of the real `google-adk` API
(`python/keel/tests/test_packs_adk.py`), proving zero-code-change plugin
auto-registration and `tool:<name>` wrapping. A live end-to-end ADK demo is
deliberately **not** built here: driving a real ADK agent turn needs either
live Gemini credentials (breaks this directory's "no real network,
deterministic" rule) or a scripted fake `google.adk.models.BaseLlm` backend
(a real chunk of extra engineering, since ADK's own model responses carry
typed `Content`/`Part`/`FunctionCall` payloads) — both out of scope for the
pack task. What a future `demos/adk-agent` should do: an ADK `LlmAgent` with
one `FunctionTool` whose body makes an `httpx.get` to a faultproxy endpoint
serving `429, 429, 200` (i.e. reuse `agent-demo`'s scenario), driven either by
a scripted fake `BaseLlm` or by invoking the registered `KeelPlugin`'s
`before_tool_callback` directly against a real `Runner` (skipping the LLM
turn) — the retry itself needs no new Keel code, since a `tool:` call's inner
`httpx.get` is already idempotent and already covered.
