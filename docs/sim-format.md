# Simulation format (`keel sim`)

Non-contract, like `docs/recording-format.md` and `crates/keel-core/src/events.rs`'s
NDJSON event feed (the precedent both follow). `keel sim` (architecture-spec §8,
sprint-plan item 5, FR7 — "deterministic simulation testing, laptop-sized") drives
a workload under a declarative **fault plan**: scripted errors, latency, and
process crashes injected at the adapter boundary, then checks the run against a
small set of pass/fail assertions.

**Scope (architecture-spec §8 is explicit about this): v1 is ADAPTER-LEVEL fault
injection, not full hermetic/wasmtime determinism.** A fault plan injects failures
into the *effect closure* an adapter hands the Tier 1 engine — exactly the seam
`tools/faultproxy` fakes over HTTP and `keel record`/`keel.testing` tee/replay over
— so Tier 1's real retry/backoff/breaker/cache logic reacts to a scripted failure
exactly as it would to a real one. It does not virtualize the whole process (no
wasmtime sandbox, no syscall interception); it does not need to, because
resilience and Tier 2 durability are already deterministic over the journal by
construction (spec §4.4) — a sim only needs to make the *fault sequence*
deterministic, which happens at this one seam.

## The fault plan (JSON)

```json
{
  "v": 1,
  "target": "pipeline.py",
  "args": [],
  "max_restarts": 4,
  "faults": {
    "api.pay.example": [
      { "kind": "timeout" },
      { "kind": "5xx", "status": 503 },
      { "kind": "ok" }
    ],
    "api.ledger.example": [
      { "kind": "ok" },
      { "kind": "crash" }
    ]
  },
  "assert": {
    "max_attempts": { "api.pay.example": 3 },
    "breaker_open": ["api.flaky.example"],
    "no_breaker_open": ["api.pay.example"],
    "flow_status": "completed"
  }
}
```

- `target` / `args` — the script `keel sim` dispatches, exactly as `keel run
  <target> [args…]` would (same interpreter dispatch via `keel-cli::run::plan`).
- `max_restarts` (default 8) — how many times `keel sim` will re-invoke
  `target` after a `crash` directive kills the child, before giving up.
- `flow_lease_ms` (optional) — when `target` is a Tier 2 flow entrypoint,
  forwarded to the child as `KEEL_FLOW_LEASE_MS`; a crash-restart sleeps
  `flow_lease_ms + 200ms` before respawning so the crashed process's flow
  lease has genuinely expired before the new process tries to re-acquire it
  (otherwise KEEL-E030). Omit for a non-flow target.
- `faults` — one **ordered directive queue per target** (the exact target string
  a policy `[target."…"]` key would match). Consumption is **per Tier 1 attempt**,
  not per logical call: the Nth attempt (across every call to that target, in
  order, counting from the plan's start — see "Crash-restart" below for how this
  survives a restart) is served the Nth directive. Once a target's queue is
  spent, every further attempt passes through to the real effect live. Each
  directive:
  - `kind`: `"ok"` (passthrough — let the real effect run; useful to interleave
    successes between faults), `"conn"` (connection failure), `"timeout"`,
    `"5xx"` (HTTP 5xx; `status` defaults to 503), `"429"` (HTTP 429; `status`
    defaults to 429), `"http"` (HTTP with an explicit `status`), or `"crash"`
    (hard-kill this process right now — see below). `retry_after_ms` is honored
    on an HTTP-shaped directive (fed to the retry schedule exactly like a real
    `Retry-After` header).
  - `delay_ms` (optional) — sleep this long before resolving the directive
    (including `"ok"` — useful for exercising a timeout policy without an error).
  - `repeat` (optional, default 1) — serve this directive for this many
    consecutive attempts before advancing (mirrors `tools/faultproxy`'s scenario
    `repeat`).
- `assert` — see "Assertions" below.

Directives are injected by the language front end (`python/keel/src/keel/_sim.py`'s
`SimBackend`, `node/keel/src/sim.mjs`'s `SimBackend`), gated on the
`KEEL_SIM_PLAN=<path to this file>` environment variable `keel sim` sets on
the child — never `contracts/`, and never active unless that variable is set.

## Where the injection seam is

The same seam `keel record`/`keel.testing` use: the boundary between an adapter
(httpx/requests/fetch/`py:`/`ts:` wrappers/…) and the `Backend.execute`/
`execute_async` call, EXCEPT that `SimBackend` wraps the **`effect` closure**
passed into `execute`, not `execute`'s return value. This is the load-bearing
difference from `RecordingBackend`/`ReplayBackend`, and it is what keeps a sim
honest: the real backend still owns the retry loop and calls the wrapped effect
once per Tier 1 attempt, so a scripted `timeout` on attempt 1 followed by `ok` on
attempt 2 genuinely exercises one real backoff wait, one real retry, and (if
configured) one real breaker observation — nothing about resilience is
bypassed or faked at a higher layer. `SimBackend` never touches `enter_flow`/
`exit_flow`/`journal_time`/`journal_random`/`report`/`layer` in the Python
front end (`__getattr__`-based delegation, mirroring `_backend._NativeBackend`
and `RecordingBackend`), so Tier 2 flow control and journal semantics are
completely untouched by fault injection there. The Node `SimBackend`
(`node/keel/src/sim.mjs`) has no equivalent passthrough for `enterFlow`/
`exitFlow`/`journalTime`/`journalRandom` yet, so `keel sim` against a Node
Tier 2 flow entrypoint currently fails with a KEEL-E005 "needs the native
core" error instead of fault-injecting transparently — documented debt, not
yet supported.

## Crash-restart

A `"crash"` directive hard-kills the whole process the instant it is consumed —
before the real effect ever runs, and (for a step inside a Tier 2 flow) *after*
the native core has already journaled that step `running` (the core does this
itself, before calling into the front end's effect callback — spec §4.3's
at-least-once guarantee), so the mechanical shape is identical to a real `kill
-9` at that exact point in the program: on resume, the crashed step is found
`running` in the journal and re-executed live (`crates/keel-core/src/flow.rs`'s
`plan_step`; conformance scenarios 16/17 are the semantic model this exercises).

The crash is a real, uncatchable `SIGKILL` sent to the process's own pid
(`os.kill(os.getpid(), signal.SIGKILL)` / `process.kill(process.pid, "SIGKILL")`)
— not a `sys.exit`/`process.exit`, which would run cleanup the simulation is
specifically trying to skip. `keel sim` detects a child that died to a
signal (vs. a normal exit) and re-invokes the SAME plan against the SAME script,
up to `max_restarts` times.

**The fault sequence survives the restart.** Each target's per-attempt cursor is
persisted to a sidecar file (`<plan path>.cursor.json`, created and updated by
`SimBackend`/`_Cursor` next to the plan — fsynced immediately after every
directive is consumed, including the one that triggers the crash) so a freshly
started process picks the fault sequence up where the crashed one left off,
rather than restarting it from directive 0 forever. `keel sim` deletes any
stale cursor file before the *first* spawn of a run, so re-running the same
plan from a clean slate is always deterministic.

## Assertions

Checked after the restart loop settles (a clean exit, or `max_restarts`
exhausted), against every event file `.keel/events/*.ndjson` written during the
sim (`keel sim` sets `KEEL_EVENTS=1` on the child so the sink is always on,
and diffs the directory before/after so events from every restart's process are
aggregated, oldest first) and, when `flow_status` is set, the newest row's
status in `.keel/journal.db`:

- `max_attempts`: `{ "<target>": N }` — a finding if any `call_end` event for
  that target recorded more than `N` attempts (retries stayed within budget).
- `breaker_open`: targets that MUST show at least one `breaker_open` event
  (the breaker tripped as configured).
- `no_breaker_open`: targets that must NOT show one.
- `flow_status`: `"completed"` (the common case), `"failed"`, or `"dead"` — the
  most-recently-updated row in `.keel/journal.db`'s `flows` table must have this
  status; a finding names the mismatch. Omit when the plan's target is not a
  Tier 2 flow.

**`max_attempts`/`breaker_open` need the native core.** The Tier 1 event sink
(`crates/keel-core/src/events.rs`) is a native-core-only feature — a sim run
entirely on the pure Python/Node stub or dev backend writes no
`.keel/events/` at all. Rather than let that silently pass an assertion it
never actually checked, `keel sim` treats "an event-based assertion was
requested but zero events were observed anywhere" as its own loud finding
(`topic: "no-events"`), naming the fix (`KEEL_BACKEND=native`). `flow_status`
is unaffected (it reads the journal, not the event feed) but is moot on a
non-native run anyway, since Tier 2 durable flows are themselves native-only.
`no_breaker_open` alone never triggers this finding — "the breaker never
opened" is trivially true when there is no event feed at all, so asserting it
over a stub run is a legitimate (if weak) no-op.

## `keel sim <plan>`

Runs the plan (see above), producing a doctor-style pass/fail report — a `ok`
boolean, the settled `exit_code`, `restarts` used, and a `findings` list (each
`{ level, topic, detail }` — the same three core fields as `keel doctor`'s
findings, though `keel sim` never emits doctor's additional `action`/`fix`
fields) — with a `--json` twin. Exit code mirrors `keel doctor`: `0` when
`findings` is empty, `2` otherwise.
