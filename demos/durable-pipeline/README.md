# durable-pipeline

The demo that sells the product (dx-spec §1 Level 2): a pipeline that survives
`kill -9` and resumes without re-firing completed steps — **config-only**, no
code changes.

```
./run.sh          # needs the native core (Tier 2 is native-only)
```

- A 10-step flow appends one line per step to a shared log (a real,
  process-external side effect).
- **Run 1** hard-crashes (SIGKILL) right before step 6 — the log has 5 lines.
- **Run 2** (after the lease expires) resumes: steps 1–5 are substituted from
  `.keel/journal.db` (their effects never re-fire — no duplicate log lines) and
  6–10 run live. The log ends at exactly 10 lines; `keel flows` shows the flow
  `completed` with 10/10 steps.

What made it durable: listing `py:pipeline:main` under `[flows]` in `keel.toml`.
That's it.

Smoke-tested by `python/keel/tests/test_resume_demo.py` (the real subprocess
`kill -9` + resume assertion; native-only, skips cleanly otherwise).
