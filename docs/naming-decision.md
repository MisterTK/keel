# Registry naming decision — memo for TK

Status: **decision required before first publish** (blocks the release
workflow's publish legs; nothing else). Date of registry verification:
2026-07-12. No names have been changed in the repo — every manifest still
carries the dx-spec §6 names, and this memo exists so the rename (or the
name-acquisition fight) is a deliberate one-commit decision, not something a
release pipeline discovers at 2am.

## The problem

dx-spec §6 promises, literally:

> `pip install keel` / `npm i -D keel` / `cargo add keel` plus `brew install keel`

Verified live against the three registries on 2026-07-12, none of the plain
names is available:

| Registry  | Name        | Status (verified 2026-07-12)                                                                              | Obtainable?                                     |
| --------- | ----------- | --------------------------------------------------------------------------------------------------------- | ----------------------------------------------- |
| npm       | `keel`      | **Active third-party product**: teamkeel, "Your production-grade backend from one file", latest **0.465.0**, actively released | No. Not squatted — a live, versioned product.    |
| PyPI      | `keel`      | Abandoned squatter: a v0.1 "kill processes" tool, no releases in years                                     | Maybe: **PEP 541** name-transfer request (slow, months, not guaranteed) |
| crates.io | `keel`      | Taken (returns 200)                                                                                         | Unlikely; crates.io has no reclamation process   |
| crates.io | `keel-core` | Taken (returns 200)                                                                                         | Unlikely; same                                   |
| PyPI      | `keel-core` | **Free** (matches crates/keel-py's current dist name)                                                       | Yes — register at first publish                  |
| npm       | `@keel/core-native` | 404 — but ownership of the **`@keel` npm scope is unverified** and plausibly claimable by teamkeel (their product is literally npm `keel`) | Risky. Assume the scope is theirs to take.       |

What is NOT affected: the CLI binary name `keel` (crates/keel-cli `[[bin]]`),
`brew install keel` (Homebrew formula names are namespaced per-tap; our own tap
can always serve `keel`), and every import name (`import keel`, `keel_core`,
`import ... from "keel"` stays whatever the dist installs).

## Options

### Option A — fight for the plain names where possible, scope elsewhere

- PyPI: file **PEP 541** for `keel` now (the squatter is abandoned; this is the
  textbook case). Publish under a fallback name meanwhile.
- npm: unobtainable, full stop. Pick a scoped name: `@keel-dev/keel` (or
  another org scope we register on npm and GitHub).
- crates.io: pick prefixed names (`keel-cli` is already free-shaped; see C).

Pros: `pip install keel` — the dx-spec promise — is eventually real on PyPI.
Cons: months of PEP 541 latency gating the "official" install line; npm and
crates.io still need renames, so we carry a name matrix anyway.

### Option B — one brand-consistent scoped/prefixed family everywhere

Pick one qualifier and use it on all three registries, e.g. `keel-run`:
PyPI `keel-run`, npm `keel-run` (or scope `@keel-run/keel`), crates.io
`keel-run` + `keel-run-core`. (Availability of the specific qualifier must be
re-verified at decision time.)

Pros: one story, no per-registry special cases, publishable this week.
Cons: dx-spec §6 must be amended everywhere; the plain-`keel` muscle memory is
lost; `keel-run` etc. is strictly worse branding.

### Option C — per-registry best-available (recommended, see below)

- **PyPI**: publish the native wheel as **`keel-core`** (free today, and
  already this repo's dist name in crates/keel-py/pyproject.toml). For the
  front end, file PEP 541 for `keel`; until it resolves, publish as
  **`pykeel`** or **`keel-run`** with `keel` becoming an alias (empty
  dependency shim) if/when PEP 541 succeeds.
- **npm**: register our own scope (**`@keel-dev`** or the org name we actually
  control on GitHub) and publish `@keel-dev/keel` + `@keel-dev/core-native`.
  Do NOT build on the `@keel` scope: it is unregistered, and teamkeel — whose
  product is npm `keel` — is the natural claimant; basing our dist names on a
  scope we might lose (or that invites a trademark fight) is a foot-gun.
  node/keel-core-native's current `@keel/core-native` name must change at the
  same moment.
- **crates.io**: `keel` and `keel-core` are gone. Publish as **`keel-cli`**
  (already the crate name; ships the `keel` binary — `cargo install keel-cli`
  gives `keel` on PATH, which is what a Rust user actually wants) plus
  `keel-engine` (for today's `keel-core`), `keel-core-api` and `keel-journal`
  if free at decision time — availability of the non-verified names must be
  re-checked. A `cargo add keel` library front end does not exist yet (keel-cli
  is a bin crate), so dx-spec's `cargo add keel` line is doubly unachievable
  and should be rewritten to `cargo install keel-cli` for v0.1 regardless.

## Recommendation

**Option C**, with the PEP 541 request for PyPI `keel` filed immediately (it
costs nothing and only Option C's PyPI leg improves if it lands). Concretely:

1. File PEP 541 for PyPI `keel` today; register PyPI `keel-core` at first
   publish (it is ours to lose).
2. Register an npm scope we control (`@keel-dev` unless TK prefers another);
   rename npm dists to `@keel-dev/keel` and `@keel-dev/core-native` in the
   publish-enabling PR.
3. crates.io: publish `keel-cli` (+ `keel-core-api`, `keel-journal`,
   `keel-engine` for the engine crate) after re-verifying availability.
4. Amend dx-spec §6 in the same PR that renames, so spec and manifests never
   disagree: `pip install keel` (post-PEP-541; `pykeel` interim), `npm i -D
   @keel-dev/keel`, `cargo install keel-cli`, `brew install keel` (own tap).

Nothing in this memo is executed yet: per the 2026-07-12 decision, all
manifests keep their current names (`keel`, `keel-core`, `keel-core-stub`,
`@keel/core-native`, crate names as-is) until TK picks an option. The only
publish-blocking safety currently in-tree is crates/keel-py's
`Private :: Do Not Upload` classifier and the fact that
`.github/workflows/release.yml`'s `publish` job has every real publish
command (`cargo publish`, `twine upload`, `npm publish`) left commented out
behind a `workflow_dispatch` input that, even when set, only runs a no-op
"Publishing is not enabled" step.
