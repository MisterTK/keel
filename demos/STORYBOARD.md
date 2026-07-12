# The 40-second asciinema (storyboard)

The README's hero demo (dx-spec §6): *flaky script fails → `keel run` → survives
→ crash mid-flow → resumes.* No architecture diagram above the fold — just the
feeling that it just worked.

This is the **shooting script**, not a recording. Record with
`asciinema rec keel-demo.cast -c "bash demos/STORYBOARD.sh"` (or type it live).
Keep the terminal at 90×24. Target ~40s. All output below is real (deterministic
via faultproxy + the durable-pipeline demo); no timestamps, no network.

---

**[0:00–0:04] Title card.** Empty prompt. Type nothing for a beat.

```
# your script talks to a flaky API. today it dies. watch.
```

**[0:04–0:12] It fails bare.** One flaky endpoint (faultproxy: 503 then 200).

```
$ python app.py
Traceback (most recent call last):
  ...
httpx.HTTPStatusError: Server error '503 Service Unavailable'
$                                    # ← non-zero exit. this is production today.
```

**[0:12–0:22] It survives under keel.** Same file. Zero edits.

```
$ keel run app.py
keel ▸ wrapped 1 call site (httpx ×1) with production defaults — `keel init` to customize
flaky ok                             # ← the 503 was retried. the script never knew.
```

*(Beat. Let "flaky ok" sit. This is the whole pitch.)*

**[0:22–0:26] Turn it durable — config only.** One line in keel.toml.

```
$ cat keel.toml
[flows]
entrypoints = ["py:pipeline:main"]
```

**[0:26–0:34] Crash it mid-run.** A 10-step pipeline; SIGKILL before step 6.

```
$ keel run pipeline.py
keel ▸ running flow py:pipeline:main [pipeline#…]
[1] Killed                           # ← kill -9. steps 1-5 done, 6-10 never ran.
```

**[0:34–0:40] Re-run. It resumes.** Steps 1–5 substituted; 6–10 finish.

```
$ keel run pipeline.py
keel ▸ replaying completed flow py:pipeline:main [pipeline#…]
PIPELINE_COMPLETE                    # ← resumed from step 6. each step ran exactly once.
$ keel flows
py:pipeline:main   completed   10/10 steps
```

**[end] Card.**

```
# zero code changes. one keel.toml. uninstall = remove the package.
```

---

## Beats to nail

- The two `app.py`/`pipeline.py` are **unedited** between the failing and
  surviving shots — the camera should make that obvious (`cat app.py` once).
- Don't explain retries on screen. The `flaky ok` line *is* the explanation.
- The resume line (`replaying`/`PIPELINE_COMPLETE`) is the money shot — hold it.

The live demos backing each beat: `demos/flaky-python` (beats 0:04–0:22) and
`demos/durable-pipeline` (beats 0:22–0:40).
