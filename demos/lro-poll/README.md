# lro-poll

The demo behind Keel v0.7.0's own release title: **the poll that looked adopted
and did nothing** (issue #128).

```
./run.sh
```

## The load-bearing fact

A *running* `google.longrunning.Operation` does not say `done: false`. proto3
JSON omits a false bool, so the body on the wire is just:

```json
{"name":"projects/demo/locations/us-central1/operations/123","metadata":{"@type":"…"}}
```

`done` appears **only** when the operation completes. So a poll block written
the obvious way —

```toml
poll = { interval = "300ms", deadline = "10s", until = { field = "done", terminal = [true] } }
```

— never polls. Keel's poll layer fails open on a parsed JSON object that does
not carry `until.field` (deliberately: a mistyped field name on a host-level
table must not create an endless loop), so it hands the still-running body
straight back on attempt one. The block validates. `keel status` attributes the
call to the target. It does nothing. That is the worst kind of wrong, because
it looks adopted.

CCR-11 added one key, `until.absent = "pending"`, which says: *for this family
of APIs, absence of the field is the pending signal.*

## The two acts

`run.sh` diffs the two policy files first, so you can see the only difference
is that key, then runs **the same `app.py`** under each — each in its own
temp directory, so the `keel.toml` at cwd is the only thing that changed.
faultproxy serves the operation as three running bodies then one terminal
body, deterministically.

1. **Without `absent`.** One upstream GET. `app.py` gets the running body back,
   and the line that trusts the poll (`op["response"]["generatedSample"]["uri"]`)
   raises `KeyError: 'response'`. The traceback is the demo: that is what code
   written against a poll policy does when the poll did not poll.
2. **With `absent = "pending"`.** Four upstream GETs — three pending, one
   terminal — and `video: gs://out/video.mp4`.

## What the receipt actually is

**Keel's exit summary has no poll counter**, and this demo does not invent one.
What it really prints is:

```
keel ▸ 1 call                          # act 1
keel ▸ 1 call · 1 retry succeeded      # act 2
```

A poll is one *call* either way — cache, rate and breaker see a single
admission, which is the point of the layer order. The poll's extra iterations
do show up, but under the `retries_succeeded` counter (`attempts > 1` on a
successful call), so the summary line reads "retry" for something that was a
poll. It makes the two acts distinguishable; it does not name what happened.

So the proof lives in two places that cannot be faked:

* `resp.keel_outcome["attempts"]` — Keel's own count of upstream attempts
  behind the one call: **1** in act 1, **4** in act 2. `app.py` prints it.
* faultproxy's own request log, which `run.sh` diffs per act: **1** GET vs
  **4** GETs. That is the server counting, not Keel.

## What this demo does *not* show

It does **not** demonstrate Keel's Google-surface inference. The endpoint is
`127.0.0.1`, which classifies as neither Vertex AI nor the Gemini API, and
`keel doctor`'s route-key proposals are provider-specific — doctor says nothing
about a poll block on loopback. This demo shows the **consequence** (`until`
without `absent` does not poll, on any host). For the surface half — doctor
proposing an `operations.get` route key with `absent = "pending"` already in
it — see `keel doctor` against a real Vertex/Gemini project, the v0.7.0 release
notes, and issue #139 (doctor still suppresses that proposal for operators who
already declared the route key).

Note also that `until.absent` sets a **version floor**: a Keel older than 0.7.0
rejects the unknown key with KEEL-E001, and under auto-activation that means
running with no Keel at all. Check your fleet before applying it.

## Backend

Poll is Tier 1, so this runs identically on the native core and the pure-Python
stub (which has slept for real since v0.6.5) — verified both ways. No native
build needed; the whole thing takes under two seconds.

Smoke-tested by `python/keel/tests/test_demos.py::LroPollAbsentTest`.
