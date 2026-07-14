#!/usr/bin/env bash
# Stage a built keel-core-native cdylib into the napi-rs-style per-platform
# prebuild layout consumed by node/keel-core-native/index.mjs and packed by
# the release workflow.
#
#   scripts/napi-prebuild.sh <target-triple> [--also-local]
#
# Reads the already-built artifact from `target/<target-triple>/release/`
# (cargo only nests output under a target/ subdirectory when `--target` was
# passed explicitly — build with, e.g.:
#
#   cargo build -p keel-node --release --target x86_64-apple-darwin
#   cross build -p keel-node --release --target aarch64-unknown-linux-gnu
#
# then run this script) and copies it to:
#
#   node/keel-core-native/npm/<platformKey>/keelrun-core-native.<platformKey>.node
#
# — the file each npm/<platformKey>/package.json's `main` points at, matching
# napi-rs's own naming convention (a future move to the real `@napi-rs/cli`
# only swaps this script, not the layout or the loader). `--also-local`
# additionally copies to node/keel-core-native/keelrun-core-native.node, the
# "canonical" single-platform dev path index.mjs checks before target/.
#
# We hand-roll this instead of installing `@napi-rs/cli` (a devDependency that
# would need a network fetch on every build) because the two things it would
# do here — invoke cargo, then rename+move one file — are exactly this
# script; see docs/gaps npm-native-prebuild-layout for the decision record.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

target="${1:?usage: scripts/napi-prebuild.sh <target-triple> [--also-local]}"
also_local=0
if [[ "${2:-}" == "--also-local" ]]; then
  also_local=1
fi

case "$target" in
  x86_64-apple-darwin) platform_key="darwin-x64"; cdylib="libkeel_node.dylib" ;;
  aarch64-apple-darwin) platform_key="darwin-arm64"; cdylib="libkeel_node.dylib" ;;
  x86_64-unknown-linux-gnu) platform_key="linux-x64-gnu"; cdylib="libkeel_node.so" ;;
  aarch64-unknown-linux-gnu) platform_key="linux-arm64-gnu"; cdylib="libkeel_node.so" ;;
  *)
    echo "napi-prebuild: unsupported target '$target' (see node/keel-core-native/package.json's napi.targets)" >&2
    exit 1
    ;;
esac

src="target/$target/release/$cdylib"
if [[ ! -f "$src" ]]; then
  echo "napi-prebuild: $src not found — build it first:" >&2
  echo "  cargo build -p keel-node --release --target $target   (or: cross build ...)" >&2
  exit 1
fi

dest_dir="node/keel-core-native/npm/$platform_key"
dest="$dest_dir/keelrun-core-native.$platform_key.node"
mkdir -p "$dest_dir"
cp "$src" "$dest"
echo "napi-prebuild: $src -> $dest"

if [[ "$also_local" -eq 1 ]]; then
  cp "$src" "node/keel-core-native/keelrun-core-native.node"
  echo "napi-prebuild: $src -> node/keel-core-native/keelrun-core-native.node (--also-local)"
fi
