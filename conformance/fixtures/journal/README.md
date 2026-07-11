# Golden journal fixtures

Checked-in `.sql` fixtures over the frozen schema (`contracts/journal.sql`)
for tooling teams (CLI `keel flows` / `keel trace` / `keel status`) to build
against before the real core writes journals. All timestamps are fixed
constants (2026-07-11 UTC) so fixture databases are bit-reproducible.

| Fixture | Story |
| --- | --- |
| `completed-flow.sql` | `py:pipeline.ingest:main` ran to completion: 5 steps, one of which (step 3) succeeded on its 2nd attempt |
| `interrupted-flow.sql` | Same entrypoint, crashed mid-step-4 (step has no outcome yet), lease expired — exactly what recovery must pick up |
| `dead-flow.sql` | `py:jobs.nightly:run` failed on every resume; step 2 poisoned it; flow status `dead` for `keel flows --dead` |

Build `.db` files (written to `.gen/`, gitignored):

```
$ python3 conformance/fixtures/journal/build_fixtures.py
```
