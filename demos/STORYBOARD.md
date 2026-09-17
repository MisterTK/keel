# The 40-second asciinema (storyboard)

The README's hero demo (dx-spec §6): *flaky script fails → `keel run` → survives
→ crash mid-flow → resumes.* No architecture diagram above the fold — just the
feeling that it just worked.

This is the **shooting script**, not a recording. Record with
`asciinema rec keel-demo.cast -c "bash demos/STORYBOARD.sh"` (or type it live).
Keep the terminal at 90×24. Target ~40s.

**The output below is transcribed from real runs of `demos/flaky-python` and
`demos/durable-pipeline`** (verified against v0.6.1). Two honest notes about
transcription:

- Keel writes its `keel ▸` lines to **stderr** and your program writes to
  **stdout**, so their interleaving is not deterministic — `flaky ok` may land
  above or below the summary. Either is real.
- The startup banner names **every** adapter it wrapped, with versions, and
  the **absolute** path of the policy it loaded, so its real length depends on
  your environment and it will usually wrap at 90 columns. A real one:

  ```
  keel ▸ wrapped httpx 0.28.1, requests 2.34.2, subprocess 3.14.7, urllib3 2.7.0, urllib.request 3.14.7 with policy /Users/you/demo/keel.toml
  ```

  The shots below abbreviate it to `…` after the first adapter so the beats
  stay readable — that elision is the one liberty taken with the transcript.
  Run in a directory with **no** `keel.toml` and the tail reads `with
  production defaults — no keel.toml in <dir>; \`keel init\` to customize`
  instead. Flow ids are likewise elided as `…`.

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

**[0:12–0:24] It survives under keel.** Same file. Zero edits.

```
$ keel run app.py
keel ▸ wrapped httpx 0.28.1, … with policy /Users/you/demo/keel.toml
flaky ok                             # ← the 503 was retried. the script never knew.
keel ▸ 1 call · 1 retry succeeded
       keel report --open for the full picture
```

*(Beat. Let the summary line sit. `flaky ok` is the promise; `1 retry
succeeded` is the receipt — Keel says what it did, unprompted, on every run.)*

**[0:24–0:28] Turn it durable — config only.** One line in keel.toml.

```
$ cat keel.toml
[flows]
entrypoints = ["py:pipeline:main"]
```

**[0:28–0:34] Crash it mid-run.** A 10-step pipeline; SIGKILL before step 6.

```
$ keel run pipeline.py
keel ▸ running flow py:pipeline:main [py:pipeline:main#…]
[1] Killed                           # ← kill -9. steps 1-5 done, 6-10 never ran.
```

**[0:34–0:40] Re-run. It resumes.** Steps 1–5 substituted; 6–10 finish.

```
$ keel run pipeline.py
keel ▸ running flow py:pipeline:main [py:pipeline:main#…]
PIPELINE_COMPLETE                    # ← resumed from step 6.
keel ▸ 10 calls
       keel report --open for the full picture
$ keel flows
keel ▸ flows: 1 total
  py:pipeline:main#…  py:pipeline:main  completed  steps 10/10  0s ago
```

*(The effects log is the proof each step ran exactly once — 5 fired before the
crash, 5 after, 10 total. `demos/durable-pipeline/run.sh` counts it on screen;
if you want that on camera, run the demo rather than typing the commands.)*

**[end] Card.**

```
# zero code changes. one keel.toml. uninstall = remove the package.
```

---

## Beats to nail

- The two `app.py`/`pipeline.py` are **unedited** between the failing and
  surviving shots — the camera should make that obvious (`cat app.py` once).
- Don't explain retries on screen. `flaky ok` plus `1 retry succeeded` *is* the
  explanation: the app got what it asked for, and Keel told you what it cost.
- The resume beat's money shot is `steps 10/10` in `keel flows`, not the
  startup line — run 2's banner is the **same** `running flow` line as run 1's.
  Keel does not announce a replay on stderr; the journal is where resume is
  visible. Don't wait for a line that isn't coming.

The live demos backing each beat: `demos/flaky-python` (beats 0:04–0:24) and
`demos/durable-pipeline` (beats 0:24–0:40).
