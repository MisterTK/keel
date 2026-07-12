# agent-demo

Agent code is the densest concentration of flaky effects — LLM calls, tool
calls, MCP round-trips (dx-spec §4). This demo models an agent's LLM completion
as an intercepted HTTP call against a fake, flaky endpoint and shows two wins
with **zero agent-code changes**:

```
./run.sh
```

1. **429-storm survival.** faultproxy serves `/v1/complete` as `429, 429, 200`.
   The `[target."127.0.0.1"]` policy (`retry = { attempts = 6 }`) rides it out;
   the agent just gets its completion.
2. **Dev-cache replay (the selfish win).** `cache = { mode = "dev" }` caches the
   completion off `KEEL_ENV=prod`. With the native core + journal, the **second
   run** of the same prompt replays from `.keel/journal.db` and makes **~0 API
   calls** — 10× faster iteration, near-zero spend while you hack on agent logic.
   `run.sh` prints the upstream call count after each run; it does not increase
   on run 2.

`keel.toml` here wraps the loopback host directly; in a real agent the same
policy targets `llm:openai` / `llm:anthropic` (auto-detected provider hosts).
Cross-run replay is native-only; without the native core the second run calls
again (still correct, just not free).

Smoke-tested by `python/keel/tests/test_demos.py::AgentDemoDevCacheTest`
(native-only; skips cleanly otherwise).
