# adk-demo

`agent-demo` proves Keel's Tier 1 resilience wraps an agent's LLM completion
with zero code changes — but it isn't an actual ADK agent, just a bare
`httpx.get`. This demo is: a real `google.adk` `LlmAgent` with one
`FunctionTool` (`fetch_answer`), driven by a scripted `BaseLlm` (no live LLM,
no network beyond loopback), whose tool body makes an ordinary intercepted
`httpx.get` against a fake, flaky completion endpoint.

```
./run.sh
```

**What it shows:** `tools/faultproxy` serves `/v1/complete` as `429, 429,
200` (`agent-demo`'s scenario, reused). The `[target."127.0.0.1"]` policy
(`retry = { attempts = 6 }`) rides out the storm *inside* the tool call —
below the agent loop. The scripted model only ever sees **one** function-call
turn and **one** final-text turn: the retries are invisible to it.

**What it proves:** 3 upstream calls happen (2×429 + 1×200), but the agent
spends exactly 1 turn invoking the tool — zero extra LLM tokens burned riding
out a rate-limit storm that, without Keel, would need agent-level retry logic
(and agent-level retries cost real tokens, since they re-run the LLM call).
`run.sh` asserts the upstream call count is exactly 3 via faultproxy's log.

If `google-adk` isn't installed, both `agent.py` and `run.sh` skip cleanly
with a hint (`pip install google-adk`) instead of failing — this demo never
crashes a stub-only checkout.

Smoke-tested by `python/keel/tests/test_demos.py::AdkDemoTest` (skips
cleanly when `google-adk` isn't importable).
