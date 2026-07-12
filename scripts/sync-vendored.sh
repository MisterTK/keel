#!/usr/bin/env bash
# Refresh every vendored copy from its single source of truth. Vendoring exists
# only so packaged artifacts are self-contained (`cargo package` / `npm pack` /
# wheels cannot ship files outside their package root); semantics never fork:
# drift fails the build (crate build.rs), the front-end test suites, and
# scripts/check-release-metadata.sh.
#
#   contracts/core_api.rs          -> crates/keel-core-api/contract/
#   contracts/journal.sql          -> crates/keel-journal/contract/
#   contracts/error-codes.json     -> crates/keel-cli/contract/
#   contracts/defaults.toml        -> crates/keel-cli/contract/
#   python/keel-core-stub/keel_core_stub/__init__.py
#                                  -> python/keel/src/keel_core_stub/
#   node/keel-core-stub/index.mjs  -> node/keel/src/vendor/keel-core-stub/
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

cp contracts/core_api.rs crates/keel-core-api/contract/core_api.rs
cp contracts/journal.sql crates/keel-journal/contract/journal.sql
cp contracts/error-codes.json crates/keel-cli/contract/error-codes.json
cp contracts/defaults.toml crates/keel-cli/contract/defaults.toml
cp python/keel-core-stub/keel_core_stub/__init__.py \
   python/keel/src/keel_core_stub/__init__.py
cp node/keel-core-stub/index.mjs \
   node/keel/src/vendor/keel-core-stub/index.mjs

echo "sync-vendored: all vendored copies refreshed"
