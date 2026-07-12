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
