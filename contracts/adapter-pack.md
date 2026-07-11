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
