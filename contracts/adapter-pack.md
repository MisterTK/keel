# Adapter-pack contract — contracts-v1

Every library adapter and framework pack (httpx, undici, `llm:openai`, ADK,
Vercel AI SDK, …) is a small, uniform module with exactly four operations plus
contract tests. This uniformity is what makes "add framework X" a bounded,
one-team task forever (DX spec §4.3).

Interface stubs: [`stubs/adapter_pack.rs`](stubs/adapter_pack.rs) ·
[`stubs/adapter_pack.py`](stubs/adapter_pack.py) ·
[`stubs/adapter-pack.ts`](stubs/adapter-pack.ts)

## The four operations

### `detect() -> Detection`
Decide, from dependencies and importable modules only (never by executing user
code), whether this pack applies. Returns the matched library/framework name,
the installed version, and a confidence: `pinned` (version is covered by
contract tests) or `best_effort` (newer/older than the pinned range — the pack
still tries, and `keel doctor` says so).

### `seams() -> [Seam]`
Declare what the pack patches and why that seam is stable — one entry per
patch point, with the upstream API it relies on (e.g. "httpx:
`HTTPTransport.handle_request`, documented transport API"). `keel doctor`
prints this verbatim; a pack whose seam it cannot explain does not ship.
Patches must be reversible (uninstall = remove the package, DX invariant 2).

### `targets() -> [TargetDecl]`
The semantic targets the pack exposes — e.g. the httpx adapter exposes
host-derived targets; the openai pack exposes `llm:openai`; an ADK pack
exposes `tool:<name>` per registered tool. Each declaration says how a call
site maps to a target id and how `idempotent` and `args_hash` are derived
(the safety judgment lives in the adapter, closest to the library).

### `defaults() -> policy fragment`
The pack's policy pack: a fragment in keel.toml JSON form (validated against
policy.schema.json) merged UNDER user config — e.g. the `llm:` pack ships
Retry-After-aware retry, dev-mode cache, and budget/fallback knobs
(contracts/defaults.toml `[defaults.llm]` is the generic layer; provider
packs may refine it).

## Idempotency-key injection

When the *effective* policy for a matching target carries `idempotency =
{ header = "..." }` (policy.schema.json `$defs/idempotency`), the adapter owns
key **injection**, not just recognition:

1. **When.** On an intercepted call with an unsafe method (one outside the
   adapter's idempotent-method set — e.g. POST/PATCH for HTTP) to a matching
   target, where the caller did not already supply the configured header (or
   a recognized default idempotency header). A caller-supplied key always
   wins; adapters never overwrite one.
2. **Minting.** The adapter mints ONE opaque key per logical call, before the
   first attempt, and injects it as the configured header on **every**
   attempt of that call. Stable across Tier 1 retries — never re-minted per
   attempt.
3. **Tier 2 stability.** Inside a durable flow the minted key is journaled
   with the step record, and a resume that re-executes a crashed (`running`)
   step injects the SAME recorded key — this is what makes at-least-once
   re-execution safe on the provider side (architecture spec NFR4, §7).
4. **Judgment flip.** An injected key flips the adapter's `idempotent`
   judgment to `true` (core_api.rs `Request.idempotent`: "idempotent method,
   or an idempotency key was injected"), so the core's retry layer applies
   instead of terminating with KEEL-E014.
5. **`args_hash` is unchanged.** The injected key is not part of the caller's
   arguments and must NOT feed `args_hash` — otherwise the Tier 2 step key
   `(target)#(args_hash)` would differ across re-executions and replay would
   fence (KEEL-E031).

## Contract tests

Each pack pins the library/framework versions it certifies and runs its seam
tests against those pins in CI (the "adapter CI farm"). A version bump that
breaks a seam turns the pack `best_effort` until re-certified — the
maintenance tax of the zero-code-changes promise, made visible instead of
wished away.

## Rules

1. A pack never imports the wrapped library at `detect()` time unless it is
   already present in the environment.
2. A pack never changes success-path semantics — wrapping is observationally
   transparent apart from resilience behavior.
3. All resilience behavior flows through the core (`keel_execute`); packs
   contain zero retry/backoff/breaker logic of their own.
4. Errors are classified into `ErrorClass` (contracts/core_api.rs) at the
   seam; the original error object is round-tripped so the front end can
   re-raise it unchanged (DX invariant 5).
