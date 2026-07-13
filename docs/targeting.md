# Keel — policy targeting syntax

*Small spec doc requested by `architecture-spec.md` §10 ("policy targeting
syntax stability — worth a small spec doc of its own since it's user-facing
API"). Normative for outbound host/URL-pattern targets (FR2) and for `[flows]
entrypoints` glob designation (§4.3). Both were resolved as **no contract
change**: the frozen `targetKey` grammar (`contracts/policy.schema.json`
`$defs.targetKey`) already admits `*` in the host/path position, and the
frozen `entrypointRef` grammar (`^(py|ts|rs):[^\s]+$`) already admits any
non-whitespace body, glob-shaped or not.*

*Resolution stays a **front-end** judgment in both cases, exactly like the
existing `llm:<provider>` host map and idempotency-header defaulting: the
front end picks one concrete policy key per call/run and hands it to the core
verbatim. `keel-core`'s `Policy::resolve` (`crates/keel-core-api/src/
policy.rs`), the three stubs, and the conformance scenarios are unchanged —
they only ever see exact keys. This is why the change needed no CCR and no
cross-implementation parity ripple beyond the two front ends that do URL/path
matching (Python, Node) and the two that do flow-glob matching (Python and
Node both implement flow designation, with slightly different concrete-
designator syntax — `:function` for Python vs `#exportName` for Node — see
"Node flow designation" below).*

## 1. Outbound host/URL-pattern targets

### 1.1 Grammar

An outbound `[target."<key>"]` key is:

```
[METHOD ]<host>[:<port>][/<path>]
```

- **METHOD** — optional, one of `GET HEAD POST PUT PATCH DELETE OPTIONS`,
  followed by exactly one space. Absent = matches every method.
- **host** — required. May contain `*` as a wildcard segment (crosses `.`,
  e.g. `*.internal.corp` matches `db.internal.corp` and
  `a.b.internal.corp`). Compared case-insensitively.
- **`:port`** — optional. Must equal the request's *effective* port: the
  explicit port in the URL, else the scheme default (80 for `http`, 443 for
  `https`). Absent = matches any port.
- **`/path`** — optional. May contain `*` as a wildcard (crosses `/`).
  Matched against the request's full path (an empty path normalizes to `/`).
  Compared case-sensitively. Absent = matches any path.

`*` is the only metacharacter in either the host or the path segment — never
full glob syntax (`?`, `[...]`, brace expansion stay literal). Patterns are
anchored end-to-end (`^...$`), so `api.*` does not match `api.example.com/v2`
unless the key itself ends in a trailing `*`.

Examples (all valid under the frozen `targetKey` pattern, no schema change):

```toml
[target."*.internal.corp"]              # every host under internal.corp
breaker = { window = "30s", failure_rate = 0.5, cooldown = "15s" }

[target."GET api.catalog.internal/*"]   # method + path glob (architecture-spec §4.1's own example)
cache   = { ttl = "10m", scope = "persistent" }

[target."api.stripe.com:443"]           # port-qualified — matches only the default HTTPS port
rate    = "90/s"

[target."api.partner.com/v1/*"]         # path glob, any method
timeout = "5s"
```

### 1.2 Resolution precedence

For one outbound call, in order:

1. **LLM host map.** A hardcoded host → semantic-target mapping
   (`api.openai.com` → `llm:openai`, etc.) always wins — it existed before
   patterns and is not a `[target]` key at all. Unaffected by this feature.
2. **Exact.** A `[target]` key that is a bare host with no method prefix, no
   `:port`, no `/path`, and no `*` — equal to the request's host. Identical
   to pre-pattern behavior; a bare-host key you already have keeps working
   unchanged.
3. **Pattern.** Every other outbound-shaped key, matched against the
   request's method + host + effective port + path. When more than one
   pattern matches, the **most specific** wins, by, in order:
   1. fewest `*` wildcards,
   2. most literal (non-`*`) characters,
   3. a METHOD prefix beats no prefix,
   4. lexicographically smallest key (a pure tiebreaker — makes selection
      total, so the same policy always picks the same key, in both
      languages, on every run).
4. **Class default.** Nothing matches: the target stays the bare hostname,
   and the core's own fallthrough (`defaults.llm` for `llm:*`, else
   `defaults.outbound`) applies exactly as it always has.

Whichever key wins becomes **the call's policy target**: it is what
`resolve_policy_target`/`resolvePolicyTarget` returns, what layer config
(`backend.layer(target, key)`) is looked up under, what circuit breaker and
rate limiter instance is shared, and what appears in `discovery` and
`keel status`/`keel explain` output. Every request a pattern key matches
therefore shares *one* breaker/rate-limiter/status line — a Keel "target" is
a policy dependency, not a per-URL bucket. The response cache is not aliased
by this pooling: its key is still `args_hash`, derived from the full
method+URL(+body), so two different URLs matched by the same pattern key
never collide in the cache even though they share breaker/rate state.

### 1.3 Implementation

Front-end judgment, one implementation per language, kept in parity:

- **Python** — `python/keel/src/keel/_targets.py`: `compile_outbound_targets`
  compiles a policy's `[target]` table into `CompiledTargets(exact, patterns)`
  once at `install_keel` time; `resolve_outbound` picks a key per call.
  `keel.adapters._http.resolve_policy_target` wraps it with the LLM host-map
  check; `httpx_pack._judge` and `requests_pack._judge` call it with the
  request's method/host/scheme/port/path.
- **Node** — `node/keel/src/judge.mjs`: `compileOutboundMatchers` /
  `resolvePolicyTarget` are the twins of the above, compiled once in
  `bootstrap.mjs` from the effective policy and consulted by `fetch.mjs`'s
  wrapped `fetch`.

Both compile once (at install/bootstrap time, from the same effective policy
the core is `configure`d with) rather than per-call, and both produce the
identical precedence order for identical policy input — a policy authored
once behaves the same in either language's `keel run`.

### 1.4 Conformance

`conformance/scenarios/` intentionally use exact target ids only (see
`conformance/README.md` §1, "glob/pattern resolution is a front-end concern
and tested separately") — the core/stub layer-resolution semantics these
scenarios pin (`target."<id>"` else `defaults.llm`/`defaults.outbound`) do
not change. Pattern *selection* is tested directly against each front end's
resolver: `python/keel/tests/test_targets.py`,
`python/keel/tests/test_adapters_http.py`, `node/keel/test/judge.test.mjs`,
`node/keel/test/fetch.test.mjs`.

## 2. Flow designation globs

### 2.1 Grammar

`[flows] entrypoints` accepts, per entry, one of:

```
py:<module>:<function>          # concrete — unchanged from before this feature
py:<module-glob>:<function>     # module may contain `*`; function stays concrete
py:<module-glob>                # shorthand: function defaults to "main"
```

The module-glob form's `*` matches any run of characters, dots included, over
the **dotted module path** a run of `keel run <script>` would import — the
same single-metacharacter dialect as outbound patterns (§1.1), so there is
one glob rule to learn across the whole policy file. The function name is
**never** allowed to contain `*`: a flow entrypoint must always name exactly
which function to run.

```toml
[flows]
entrypoints = [
  "py:pipeline.ingest:main",   # concrete, as always
  "py:jobs.*:run",             # any dotted module directly under jobs.*, function run
  "py:pipeline.*",             # shorthand for "py:pipeline.*:main"
]
```

A concrete `py:<module>` with no `:function` is still skipped (unchanged): a
non-glob designation with no function is ambiguous, not defaulted.

### 2.2 Matching and identity

`match_flow(target, entrypoints)` (`python/keel/src/keel/_flow.py`) decides
whether the script passed to `keel run <target>` is a designated flow:

1. **Concrete entries are tried first**, in declaration order, exactly as
   before glob support existed: a dotted module matches only the file path
   suffix it names; a single-component module matches any file with that
   stem.
2. **Glob entries are tried second.** The target script's path is read as a
   dotted module in every way that could plausibly be `import`ed: built from
   the file stem outward (`ingest`, then `pipeline.ingest`, then
   `demo.pipeline.ingest`, …), stopping at the first path component that
   is not a valid Python identifier (such a component could never be an
   importable package). Glob entries are tried in declaration order; for
   each, its candidates (shortest first) are tested against its regex; the
   first entry with any matching candidate wins.
3. **No match at either tier** — the script runs as a plain script, not a
   flow (unchanged Tier-1-only behavior).

Concrete-before-glob, in declaration order, is a deliberate, deterministic
tie-break: an explicit designation always beats an incidental glob hit, so
narrowing a glob's exceptions with a concrete entry never requires reordering
the list.

**Flow identity never contains a glob.** When a glob entry matches, the
`FlowEntrypoint` returned by `match_flow` is resolved to the *matched
concrete module* — `raw = "py:<resolved-module>:<function>"` — with `via`
carrying the original declared glob for diagnostics. `run_as_flow` opens the
journal under this resolved `raw`, so two different scripts matched by the
same glob (`py:jobs.a:run` and `py:jobs.b:run` both matching `py:jobs.*:run`)
get two independent flow identities and journals, never one shared/colliding
identity. This mirrors why outbound pattern keys (§1) become the call's
target verbatim — a pattern is a *selector*, never itself an identity.

### 2.3 Node flow designation

Node implements `[flows]` entrypoint matching and a durable-flow runner too:
`node/keel/src/flow.mjs`'s `matchFlow` (concrete entries before glob entries,
mirroring §2.2) and `runAsFlow` (enters/exits the flow, journals time/random
via the native backend, and always calls `process.exit`); entries are parsed
by `node/keel/src/policy.mjs`'s `extractFlowEntrypoints`; `node/keel/hook.mjs`
(preloaded by `node/keel/bin/keel-node-run.mjs`) dispatches to them. Tested in
`node/keel/test/flow.test.mjs`.

Node's grammar mirrors Python's but uses `#` rather than `:` before the
function name, matching the existing `[target."ts:..."]` convention:

```
ts:<pathGlob>#<exportName>
```

One real difference from Python: the export name must always be concrete —
there is no shorthand defaulting to `main` the way Python's bare
`py:<module-glob>` does.

## 3. Cross-language parity contract

Both features are governed by one rule, restated from
`conformance/README.md` and `CLAUDE.md`: the **glob dialect** (`*` only,
crossing `.` and `/`, anchored end-to-end) and the **precedence rules** in
§1.2 and §2.2 are normative across every implementation that does this kind
of matching. A future change to either — a new metacharacter, a different
specificity order — is a semantics change under the parity rule and must
update this document plus every implementation (today: `python/keel`,
`node/keel`) plus their tests together, the same discipline used for
core/stub Tier 1/Tier 2 semantics.
