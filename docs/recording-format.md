# Recording format (`keel record`)

Non-contract. `keel record` (dx-spec §6 standing structure item 5, "record
test") captures every effect a program made under `keel record run` and lets
`keel record test` turn that capture into an offline, deterministic test
fixture. Everything on this page lives entirely in the front ends
(`python/keel/src/keel/_record.py`, `python/keel/src/keel/testing.py`,
`node/keel/src/record.mjs`, `node/keel/src/testing.mjs`) plus the CLI
(`crates/keel-cli/src/record.rs`) — **not** `contracts/`. The line format is
versioned (`"v": 1`) but Keel's own tooling is the only reader; a CCR is never
needed to change it (compare `crates/keel-core/src/events.rs`'s NDJSON event
feed, the precedent this format follows).

## Why NDJSON, not the `.db` sketched in the gap brief

The brief's illustrative name (`.keel/recordings/<id>.db`) suggested SQLite.
This implementation uses newline-delimited JSON instead:

- Both front ends must be able to *write* a recording as the program runs.
  Python's stdlib `sqlite3` makes that easy; Node has no batteries-included
  SQLite binding without either a new native dependency or gating on a recent
  `node:sqlite` (still edging out of experimental across the `engines` range
  `keel` supports). NDJSON needs nothing beyond `fs`.
- It is exactly the precedent `.keel/events/<run>.ndjson` (the Tier 1 event
  sink) already set for "non-contract, append-only, tooling-only" data.
- It is trivially appendable (no transaction/locking story needed for a
  single-writer, single-reader file) and human-inspectable with `cat`/`jq`.

## File layout

`.keel/recordings/<id>.ndjson`, one recording per file. `<id>` is a
zero-padded-hex epoch-millisecond string (same convention as the event sink's
run ids — lexically sortable, newest last).

### Line 1: the `meta` header (always first, always present)

```json
{"v":1,"type":"meta","id":"<id>","language":"python"|"node","target":"<script path>","args":["..."],"started_at_ms":1752345600000,"redacted_headers":["authorization","x-api-key","..."]}
```

### Lines 2+: one `call` record per intercepted effect

One record per logical `backend.execute`/`execute_async` invocation — i.e.
per wrapped call site invocation, **not** per Tier 1 retry attempt (a call
retried 3 times before succeeding is still one `call` record; `attempts`
carries the count).

```json
{"v":1,"type":"call","seq":1,"target":"api.example.com","op":"GET api.example.com/users","idempotent":true,"args_hash":"3b1c...","attempts":1,"latency_ms":12,"body_captured":true,"outcome":{"v":1,"result":"ok","payload":{"__keel_http__":1,"status":200,"headers":[["content-type","application/json"]],"body_b64":"eyJvayI6dHJ1ZX0="},"attempts":1,"from_cache":false,"waits_ms":[],"throttled":false,"throttle_wait_ms":0,"breaker":"closed","trace_id":"t-000001"}}
```

`outcome` is the **exact envelope the backend returned** to the adapter
(`Outcome` in `contracts/core_api.rs`), captured *before* the adapter attaches
anything of its own (e.g. a live exception under `error.original`) — so it is
always plain JSON, never a live object. `body_captured` is `true` iff
`outcome.payload` carries an actual body (`body_b64` for an HTTP envelope, or
any non-null payload for a `py:`/`ts:` function target).

## Where the capture seam is

The tee sits at the **one seam every adapter already funnels through**: the
`Backend.execute`/`execute_async` boundary between an adapter (httpx,
requests, `py:`/`ts:` function wrappers, …) and the Tier 1 engine
(`python/keel/src/keel/_backend.py`'s `Backend` protocol;
`node/keel/src/backend.mjs`'s equivalent). A `RecordingBackend` wraps the real
backend, forwards every call unchanged, and appends what it saw — a pure
observer. **Recording never changes program behavior**: the wrapped call
receives exactly the outcome the real backend produced, and nothing about
retry/cache/breaker decisions is altered by recording being on. This is the
same transparency invariant `KEEL_DISABLE` and the dev cache already promise
(dx-spec invariant 5).

Enabled via `KEEL_RECORD=<path>` in the child's environment — set by `keel
record run <script>`, which is otherwise identical to `keel run <script>`
(same dispatch, same `--disable`-style transparency on every other axis).

## Request-matching semantics (used by `keel record test` / replay)

A replay backend (`keel.testing.ReplayBackend`,
`node/keel/src/testing.mjs`'s `ReplayBackend`) serves recorded outcomes to a
running program instead of performing real effects. The matching rule is
identical in both languages and deliberately simple:

1. Group every recorded `call` by a match key:
   - `(target, args_hash)` when the call's `args_hash` is not `null`.
   - `(target, "op:" + op)` when `args_hash` is `null` (a non-idempotent call,
     e.g. a plain `POST`, never gets an `args_hash` — see
     `python/keel/src/keel/adapters/_http.py`'s `derive_args_hash`).
2. Each group is a **FIFO queue**: a live call computes the same key from its
   own `(target, args_hash, op)` and is served the **oldest unconsumed**
   recorded outcome in that group. This makes repeated identical calls (e.g.
   the same idempotent GET fired twice) replay in the order they were
   recorded, without needing a counter the caller manages.
3. A live call whose key has no recorded queue left (never recorded, or the
   queue is already exhausted) is an **unmatched effect** — replay fails
   loudly (`keel.testing.UnmatchedEffect` in Python,
   `UnmatchedEffectError` in Node) naming the target/op/args_hash. Replay
   **never** falls through to a live call: an unrecorded effect is always a
   test failure, not a silent pass-through.

Only `target`, `op`, and `args_hash` participate in matching — never raw
request bodies/headers/args, none of which cross the `Backend.execute` seam
in the first place (the envelope sent to the core is `{target, op,
idempotent, args_hash}`; see `_http.build_request`). This is a deliberate,
documented v1 floor, not an oversight — see "Known limitations" below.

A served "ok" outcome is returned with `from_cache` forced `true`
(regardless of what it was recorded as), never mutating the file on disk.
Replay never runs a real effect, so there is no live response object for an
adapter to hand back byte-transparently the way a real call would — the HTTP
packs' delivery logic (`_http.deliver` in Python, the tail of
`fetch.mjs`'s wrapper in Node) already know how to serve a **cache hit**
purely from a payload envelope with no live object, which is exactly
replay's situation.

## Redaction

Only the recorded **outcome's** HTTP response envelope (`payload.headers`,
present for `host:`/`llm:` targets) is scanned for secrets: any header whose
name case-insensitively matches the redact set has its value replaced with
`"[REDACTED]"` before the line is written (never after — an unredacted value
is never on disk even transiently). The default redact set is:

```
authorization, proxy-authorization, cookie, set-cookie,
x-api-key, api-key, x-auth-token, x-goog-api-key
```

Extend it per-run with a comma-separated `KEEL_RECORD_REDACT_HEADERS` env var
(merged with, never replacing, the defaults). The active set is written into
the recording's `meta.redacted_headers` for audit.

There is no request-body/response-body content redaction (a recorded body is
written verbatim once buffering happens — see below) — treat a recording like
you would an HTTP proxy capture: don't record against a live account holding
data you wouldn't want in your repo's `.keel/`, and keep `.keel/recordings/`
out of version control (`keel init` already `.gitignore`s `.keel/`).

## Known limitations (v1 floor, documented rather than silently missing)

- **Body capture depends on the adapter's own buffering decision.** The
  `Backend.execute` seam only ever sees what the *adapter's* effect closure
  already decided to buffer into `payload` — recording does not (and, to stay
  a pure behavior-preserving observer, must not) force additional buffering.
  In practice this means a response body is captured for: an idempotent `GET`
  to a `host:` target with `cache.ttl` configured, and any `llm:` POST (the
  dev cache defaults `cache.ttl` on outside `KEEL_ENV=prod`, so LLM calls —
  arguably the highest-value case for "replay this in a test" — are captured
  by default). A plain unconfigured `POST`/`PUT` to a REST API is still
  recorded and still replays correctly (status, error class, `attempts`), but
  `body_captured` will be `false` and the replayed outcome carries no
  `payload` body. `keel record list` reports `body_captured` counts so this
  is visible, not silent. **Natural extension** (deferred): a
  `--capture-bodies` flag that overrides the adapter's cache-buffering gate
  for the duration of a recording run only.
- **`tool:`/MCP/`llm:` target replay is exercised by the same generic seam**
  (any adapter that calls `Backend.execute` is captured/replayed identically
  — there is nothing target-kind-specific in `RecordingBackend`/
  `ReplayBackend`), but only HTTP-shaped (`host:`/`llm:`) effects were
  exercised end-to-end for this v1. Treat non-HTTP target replay as
  plausible-but-unverified rather than a documented floor.
- **Python replay auto-arms every dynamic-lookup adapter** (httpx, requests,
  urllib3, aiohttp, boto3, psycopg — everything that reads
  `keel._runtime.get_backend()` per call) via one `install_adapters()` call.
  **Node replay only auto-arms the global `fetch` seam** (`installFetch`
  captures its backend by closure at install time, unlike Python's
  dynamic-lookup packs) plus `ts:` function targets (which do read the
  runtime dynamically, like Python). Node's `pg`/`ioredis`/`mysql2`/MCP packs
  are not rewired for replay in v1 — each captures its own backend reference
  at install time the same way `fetch` does, and rewiring all of them is
  deferred (documented, not silently skipped).
- **`py:`/`ts:` function-target replay requires the function to already be
  wrapped** (a `keel.toml` `[target]` entry, or the import hook otherwise
  active) — replay does not itself discover or wrap functions from a bare
  recording; it only supplies the backend those wrappers call into.
- **Tier 2 durable flows are not specially handled.** Effects made from
  inside a flow body still route through `Backend.execute` and are captured
  like any other call, but recording does not annotate flow/step boundaries,
  and flow resume/replay machinery is untouched by any of this.

## `keel record` CLI

- `keel record run <script> [args...]` — like `keel run`, but sets
  `KEEL_RECORD=<fresh path under .keel/recordings/>` in the child's
  environment before dispatch. Otherwise identical (same interpreter
  dispatch, same exit-code passthrough); recording is invisible on the
  child's stdout/stderr except for one `keel ▸ recording to …` line the front
  end prints once at bootstrap (suppressed by `KEEL_QUIET`, matching the
  existing wrapped-banner convention).
- `keel record list` / `--json` — one row per file under
  `.keel/recordings/*.ndjson`: `id`, `language`, `target`, `args`,
  `started_at_ms`, `calls`, `body_captured`, `errors`. Newest first.
- `keel record test <recording> [--out DIR]` — `<recording>` is an id, a path,
  or an unambiguous id substring. Generates one ready-to-run test file next to
  the recording (or under `--out`): a pytest fixture (`test_<id>_replay.py`)
  for a Python recording, a `node:test` file (`<id>.replay.test.mjs`) for a
  Node recording. Both import the reusable replay helpers
  (`keel.testing`/`keel/testing`) rather than duplicating matching logic per
  generated file — regenerate freely after re-recording.
