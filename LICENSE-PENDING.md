# License — pending

Keel does not yet carry a public license. **The license choice is an open
question reserved for the project owner (TK)** and is deliberately unmade in this
repository — see `docs/architecture-spec.md` §10 ("Open questions"), which frames
it as *"Apache-2.0 for reach vs. BSL for defensibility — decide before v0.1,
repapering later is painful,"* consistent with NFR6 (§2): *"Permissive or
source-available license chosen by TK."*

Until that decision is made and a `LICENSE` file lands, treat this code as **all
rights reserved**: no rights are granted by mere access to the repository.

Package metadata reflects this honestly rather than asserting a license the repo
does not back:

- `node/keel/package.json` declares `"license": "UNLICENSED"` (npm requires a
  value; this is the honest one for an undecided license — it is *not* a claim of
  any particular terms).
- The Rust workspace/crate manifests and the Python `pyproject.toml` files
  intentionally omit a `license` field rather than assert one.

When the license is chosen, replace this file with the actual `LICENSE`, set the
real SPDX identifier in `node/keel/package.json`, and add `license`/classifier
metadata to the Cargo and Python manifests in the same change.
