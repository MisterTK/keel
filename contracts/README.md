# contracts/ — the frozen interfaces (contracts-v1)

Everything in this directory is a **compatibility promise**. Six teams build
against these files in parallel; integration works only if nobody moves them
unilaterally.

| Artifact | What it freezes |
| --- | --- |
| `policy.schema.json` | The complete keel.toml schema: target grammar, per-layer config, flows, journal |
| `schedule-grammar.ebnf` | The retry-schedule algebra (`exp(...)`, `fixed(...)`, `upTo`, `andThen`) |
| `defaults.toml` | The smart-defaults pack that ships in the binary (Level 0 behavior) |
| `core-ffi.h` | The C ABI: `keel_configure` / `keel_execute` / `keel_report`, error enum, envelope rules |
| `core_api.rs` | The normative envelope types (Request, AttemptResult, Outcome, Report) and the `KeelCore` trait — included verbatim by the `keel-core-api` crate |
| `error-codes.json` | The KEEL-E0NN taxonomy with what/why/next copy (`keel explain` corpus) |
| `journal.sql` | The SQLite journal schema (golden fixtures live in `conformance/fixtures/journal/`) |
| `adapter-pack.md` + `stubs/` | The `detect/seams/targets/defaults` pack contract in Rust/Python/TS form |

## Change process (CCR)

Nobody edits `contracts/` directly. To change a contract:

1. File a **contract-change request**: an issue describing the change, the
   teams affected, and the migration.
2. The orchestrator arbitrates and, if approved, applies the
   `contract-change-approved` label to the PR carrying the change.
3. CI (`contract-freeze` job) fails any PR that touches `contracts/` without
   that label.

## Conformance is the referee

"Done" for any component means green on `conformance/` (see its README for
the exact execution semantics every implementation must honor), not "the
author says it works."
