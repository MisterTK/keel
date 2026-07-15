#!/usr/bin/env bash
# Stage a built `keel` CLI binary into the per-platform npm prebuild layout
# consumed by node/keel-cli/bin/keel.mjs and packed by the release workflow.
# The npm analogue of scripts/napi-prebuild.sh, for keelrun-cli instead of
# keelrun-core-native.
#
#   scripts/cli-prebuild.sh <target-triple>
#
# Reads the already-built binary from `target/<target-triple>/release/`
# (built via, e.g.:
#
#   cargo build -p keelrun-cli --release --target x86_64-apple-darwin
#   cross build -p keelrun-cli --release --target aarch64-unknown-linux-musl
#
# then run this script) and copies it to:
#
#   node/keel-cli/npm/<platformKey>/keel[.exe]
#
# — the file each npm/<platformKey>/package.json's `main`/`files` point at.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

target="${1:?usage: scripts/cli-prebuild.sh <target-triple>}"

case "$target" in
  x86_64-apple-darwin) platform_key="darwin-x64"; bin_name="keel" ;;
  aarch64-apple-darwin) platform_key="darwin-arm64"; bin_name="keel" ;;
  x86_64-unknown-linux-musl) platform_key="linux-x64-musl"; bin_name="keel" ;;
  aarch64-unknown-linux-musl) platform_key="linux-arm64-musl"; bin_name="keel" ;;
  x86_64-pc-windows-msvc) platform_key="win32-x64"; bin_name="keel.exe" ;;
  *)
    echo "cli-prebuild: unsupported target '$target' (see node/keel-cli/package.json's optionalDependencies)" >&2
    exit 1
    ;;
esac

src="target/$target/release/$bin_name"
if [[ ! -f "$src" ]]; then
  echo "cli-prebuild: $src not found — build it first:" >&2
  echo "  cargo build -p keelrun-cli --release --target $target   (or: cross build ...)" >&2
  exit 1
fi

dest_dir="node/keel-cli/npm/$platform_key"
dest="$dest_dir/$bin_name"
mkdir -p "$dest_dir"
cp "$src" "$dest"
chmod +x "$dest" || true # no-op on Windows runners; harmless there
echo "cli-prebuild: $src -> $dest"
