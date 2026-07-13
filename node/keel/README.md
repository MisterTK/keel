# keel (Node front end)

Production-grade resilience for Node with **zero code changes**. Add the hook,
change nothing in your app, and every intercepted call gets retries, backoff,
circuit breaking, and rate limiting from one `keel.toml` (or conservative
built-in defaults). This is the Node twin of the Python front end; both drive
the same core semantics (see `../keel-core-stub`, `../../conformance`).

> Status: v0.1 front end on the pure-Node backend (`AsyncEngine`, a parity-tested
> async port of `keel-core-stub`). The native async core swaps in transparently
> once built (`keel-core-native`) — see "Backend".

## Use it

```bash
# ESM
node --import keel/hook app.mjs
# CJS (best-effort preload; prefer --import when you can)
node --require keel/hook app.cjs
# or via the bin the `keel run` CLI dispatches to:
keel-node-run app.mjs
```

On startup Keel prints one line to **stderr** (never stdout) and is otherwise
silent on success:

```
keel ▸ wrapped global fetch + 2 function targets with policy keel.toml — `keel init` to customize
```

Requires **Node ≥ 22.5** (for `node:sqlite`, used by discovery). Zero runtime
dependencies.

## What gets wrapped

### HTTP — global `fetch`

Every `fetch`/undici call through the global binding is intercepted. We wrap
`globalThis.fetch` (not undici's `setGlobalDispatcher`) deliberately: the
zero-dependency rule forbids the `undici` package and Node exposes no public
`node:undici` builtin, while global `fetch` is the stable seam that all modern
Node HTTP flows through. The effect forwards to the **original** `fetch`, so
uninstalling restores byte-identical behavior.

Judgments (kept in parity with the Python adapters):

- **Target** = the URL hostname, unless it maps to an LLM provider, then the
  semantic target `llm:<provider>`. The host→provider map (`LLM_HOST_PROVIDERS`)
  is a frozen cross-language parity contract: `api.openai.com → llm:openai`,
  `api.anthropic.com → llm:anthropic`,
  `generativelanguage.googleapis.com → llm:google-genai`.
- **Idempotency** — `GET/HEAD/OPTIONS/PUT/DELETE/TRACE` are retryable (TRACE is
  in the judgment table for parity even though WHATWG fetch forbids sending it).
  `POST/PATCH` are **not** retried (Level 0 hard rule) unless an idempotency
  header is present (the target's configured `idempotency.header`, or a common
  default). A non-idempotent transient failure is *observed, not retried* →
  **KEEL-E014**.
- **args_hash** (cache/journal key material) is derived **only for idempotent
  GET requests** (sha256 over method+URL); it is `null` for everything else.
- **Transient vs. success** — only `429` and `≥500` are treated as retryable
  typed errors (`Retry-After` is parsed to ms and overrides the backoff:
  `wait = max(schedule, retry_after)`). **Every other status** (2xx/3xx and
  non-429 4xx) is passed through **unchanged** — Keel never turns a real HTTP
  response into a failure.
- **Returned response** — the actual `Response` object is returned, body intact
  (we only ever read status + `Retry-After`, never the body). After exhausting
  retries on a transient status, the **last real response** is returned (not
  thrown) so the app still sees its 503. Superseded/dangling response bodies are
  cancelled so retries never leak.
- **Thrown transport errors** (`conn`/`timeout`) are re-thrown **unchanged**
  (same object identity) after the final attempt.
- Per-attempt timeout (from `timeout`) is enforced via `AbortController` **only
  for idempotent calls** — we never inject a new thrown error into a
  non-idempotent success path.

### Function targets — `ts:` in the ESM loader

A `ts:<pathGlob>#<exportName>` target in `keel.toml` wraps a module's named
function export so it routes through the backend. Registered off-thread via
`module.register`; the transform rewrites, on the main thread:

```js
export async function run(...) { <body> }
// becomes:
async function __keel$run(...) { <body> }              // body byte-identical
export const run = __keel$wrap("ts:jobs/*.mjs#run", __keel$run);
```

**Matching rules (v0.1):** `<pathGlob>` (`*` = non-`/`, `**` = any, `?` = one
char) is matched against the module's path relative to cwd *and* its basename;
`<exportName>` is the export to wrap. **Supported forms** (a documented
simplification): `export function NAME` and `export async function NAME`.
**Not** wrapped in v0.1 (left untouched, unchanged behavior): arrow/const
exports, `export { NAME }` lists, `export default`, class methods, and a target
with no `#exportName`.

Listing a `ts:` target in `keel.toml` is your assertion that the function is
**safe to retry** (so it is treated as idempotent). A thrown function error is
class `other`, which is **not** in the default `retry.on` — so by default
function failures propagate unchanged; add `"other"` to that target's `retry.on`
to retry them.

## AI agent packs

Agent code is the densest concentration of flaky effects (LLM calls, MCP
round-trips), so Keel treats those seams as first-class (DX spec §4). Every pack
follows the uniform adapter-pack contract (`detect/seams/targets/defaults`,
`../../contracts/adapter-pack.md`) and carries **zero** resilience logic of its
own — all behavior flows through the core.

### `llm:` provider defaults + dev cache

Any call that resolves to an `llm:<provider>` target — the fetch host map
(`api.openai.com → llm:openai`, …) or the AI SDK middleware below — inherits the
`[defaults.llm]` pack: 120s timeout, 6 retries on `429/5xx/timeout` with
Retry-After-aware backoff, a per-provider breaker, and a **dev response cache**.

- **Merge semantics.** The pack defaults are merged **under** your `keel.toml`
  (`applyPackDefaults`): anything you don't set still gets Level 0 protection,
  and a key you *do* set replaces that key's default wholesale (the same per-key
  precedence the core uses to resolve `target → defaults.llm → defaults.outbound`).
- **Dev cache** (`cache = { mode = "dev" }`). Identical prompt+params replay from
  cache during development (10× faster iteration, ~0 API spend). It resolves to a
  concrete cache directive off-prod and is **inert when `KEEL_ENV=prod`**
  (`resolveDevCache`). This behavior is identical to the Python front end.

### Vercel AI SDK — `keel/ai-sdk`

The AI SDK's `wrapLanguageModel` is the cleanest seam of any framework, so Keel
plugs in through its own middleware hook — the one place "zero code changes" is
spent on a framework's blessed extension point:

```js
import { wrapLanguageModel } from "ai";
import { keelMiddleware } from "keel/ai-sdk";

const model = wrapLanguageModel({ model: base, middleware: keelMiddleware() });
// use `model` anywhere; every generate/stream is now resilient.
```

- **Target** = `llm:<provider>` from the wrapped model's provider id (the base
  segment, e.g. `openai.chat → llm:openai`).
- **`wrapGenerate`** routes `doGenerate()` through the backend; a thrown provider
  error is classified (429/5xx retried per policy, `Retry-After` honored) and the
  result is dev-cache-eligible.
- **Streaming rule.** `wrapStream` wraps stream **establishment** (the
  `doStream()` call), **not chunks**: resilience applies to getting the stream
  started; once established, the stream is returned **unchanged** and its chunks
  flow through untouched — Keel never buffers, observes, or retries mid-stream
  (a live stream is not replayable), so streams are never dev-cached.

The real `ai` package is **not** a dependency; the middleware only implements the
`LanguageModelV2` middleware shape (pinned in `fixtures/ai-sdk-model.d.ts`,
mirroring `ai@5.0.0`).

**All four core generation ops are covered by these two hooks.** `ai`'s
`LanguageModelV2Middleware` exposes exactly `wrapGenerate`/`wrapStream` —
`generateObject`/`streamObject` are built on the same `doGenerate`/`doStream`
calls as `generateText`/`streamText` (object mode is a `responseFormat` value
inside `params`, opaque to Keel except as a dev-cache key), so there is no
third or fourth middleware hook to implement. The streaming rule above applies
identically to `streamObject`: retried only before the first chunk/token,
never mid-stream.

### MCP transports — `mcp:<server>`

When the MCP client SDK (`@modelcontextprotocol/sdk`) is present, the bootstrap
auto-wraps `Client.prototype.request` — the JSON-RPC request/response
correlation boundary shared by **all** transports (stdio + streamable HTTP), so
one seam covers both. Each request routes through the backend as target
`mcp:<server-name>` (`client.getServerVersion()?.name`), taking per-server
`timeout`/`retry`/`breaker` from `[target."mcp:<server>"]` or `[defaults.outbound]`.

- **Idempotency is judged from the JSON-RPC method** (Level 0 hard rule), not
  hardcoded: read-ish methods (`initialize`, `ping`, `*/list`, `resources/read`,
  `prompts/get`, `completion/complete`) are retried per policy; **`tools/call`
  and any unknown method are observed, not retried** (KEEL-E014) — the MCP
  analogue of the fetch seam's POST model, so a side-effecting tool is never
  auto-retried in v0.1 (no per-method opt-in surface is invented). Calls are
  **not cached** (potentially side-effecting).
- A **hung server on a read-ish (idempotent) call** times out per policy (the
  pack imposes a per-attempt deadline and passes an `AbortSignal` into the
  request), retries per policy, and finally raises **KEEL-E010** — it degrades
  gracefully instead of freezing the agent. As with the fetch seam, Keel does
  **not** impose its timeout on non-idempotent calls (e.g. `tools/call`) — it
  never injects a thrown timeout into a possibly-succeeding side-effecting call;
  the SDK's own request timeout is the backstop there.
- The patch is reversible (`uninstall = remove the package`). The real SDK is not
  a dependency; the wrapped shape is pinned in `fixtures/mcp-client.d.ts`
  (mirroring `@modelcontextprotocol/sdk@1.29.0`).

### Vercel eve — `tool:<name>`

[eve](https://github.com/vercel/eve) is filesystem-first: an agent is a
directory of `tools/`/`skills`/`channels`/`schedules`, and each tool is a
module built with eve's own `defineTool()` helper:

```ts
// agent/tools/get_weather.ts
import { defineTool } from "eve/tools";
export default defineTool({
  description: "…",
  async execute({ city }) { /* … */ },
});
```

eve reaches the world two ways, and Keel covers both, with zero eve-specific
code needed for the first:

- **MCP round-trips** — eve talks to MCP servers through the same
  `@modelcontextprotocol/sdk` `Client` the `mcp:` pack above already patches,
  so they're wrapped the moment that SDK is detected, eve or not.
- **Tool modules** — when eve is detected, the bootstrap arms an ESM loader
  rewrite (the same mechanism `ts:` function targets use) that intercepts the
  canonical `import { defineTool } from "eve/tools"` line in a tool module and
  wraps its `execute` function, so it routes through the backend as target
  `tool:<name>` (`<name>` = the tool file's basename) before eve ever calls it.
  Only that exact, unaliased import form is rewritten — anything else is left
  untouched (a documented v0.1 simplification, like `ts:` targets).

`tool:` calls are **non-idempotent by default** — unlike a `ts:` target (where
listing it in `keel.toml` is itself the safety assertion), eve discovers tools
automatically with no such opt-in, so a failure is observed, not retried
(KEEL-E014), mirroring the `mcp:` pack's `tools/call` default; a target may
still opt in via its own `retry.on`. Never dev-cached (tool calls can be
side-effecting).

Keel does **not** wrap eve's own conversation-level durability/checkpointing —
it hardens the effects *inside* each step, which is a different layer.

## Errors and the `keelOutcome` attachment

On final failure the original error/response propagates unchanged, with a
non-enumerable `keelOutcome` property attached (the core Outcome envelope: code,
class, attempts, waits, breaker, trace_id). Non-enumerable means JSON and
iteration are unaffected — you only see it if you look.

*Caveat:* identity is preserved for in-process calls (same object re-thrown). If
a value ever crosses a worker/`structuredClone` boundary, identity is not
preserved and the attachment (like any non-enumerable/own-symbol data) is
dropped by the clone — expected, documented.

## Discovery — `.keel/discovery.db`

An always-on, cheap aggregate of observed traffic (DX §2), written with Node's
builtin `node:sqlite` (`DatabaseSync`, zero native deps). Each intercepted call's
Outcome is folded into a per-target aggregate in memory on the hot path and
written once at exit. The table is the **canonical discovery schema** owned by
`crates/keel-journal` (`src/discovery.rs`) — byte-for-byte the same across the
Rust core and both front ends, so one `.keel/discovery.db` is inspectable the
same way everywhere. Best-effort: discovery never throws into, slows, or adds
output to your program. Schema (WAL, `WITHOUT ROWID`, single self-contained
UPSERT):

```sql
CREATE TABLE discovery (
  target            TEXT PRIMARY KEY,
  calls, attempts, retries, successes, failures,
  cache_hits, throttled, breaker_opens INTEGER NOT NULL DEFAULT 0,
  total_latency_ms  INTEGER NOT NULL DEFAULT 0,  -- Σ latency, for the mean
  max_latency_ms    INTEGER NOT NULL DEFAULT 0,
  first_seen_ms     INTEGER NOT NULL,
  last_seen_ms      INTEGER NOT NULL,
  last_error_class  TEXT,                         -- most recent error's class
  last_error_status INTEGER                       -- …and its HTTP status
) WITHOUT ROWID;
```

Accounting matches the canonical store: a cache hit is a `call` and a `cache_hit`
only (no upstream attempt), so `calls == successes + failures + cache_hits`;
`breaker_opens` counts calls that saw an OPEN breaker (fail-fast, KEEL-E012);
`last_error_*` is never erased by a later success.

## `KEEL_DISABLE=1`

Byte-identical to running with **no hook at all** — no wrapping, no loader, no
discovery, no banner, no `node:sqlite` load. `uninstall = remove the package.`

## Backend selection (`KEEL_BACKEND`)

Isolated in `src/backend.mjs`. Priority: the native addon `keel-core-native`
when loadable (probed by dynamic import; may not exist yet), else the in-repo
`AsyncEngine`. Override with `KEEL_BACKEND=stub` (force engine) or
`KEEL_BACKEND=native` (require the addon; **KEEL-E040** if missing).

## Config errors are fatal

A missing `keel.toml` falls back to Level 0 defaults silently. A *present but
broken* `keel.toml` (bad syntax, unreadable, or invalid policy) fails loudly
with **KEEL-E001** (with a line number / field path) and the app does not run —
a broken policy is a bug to fix, not a surprise to paper over.

## Tests

```bash
cd node/keel && node --test
```

Determinism: the engine takes an injectable clock, so tests use a virtual clock
(instant backoff, recorded `waits_ms`); HTTP tests hit local scripted
`node:http` servers only (no external network).
