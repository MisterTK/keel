#!/usr/bin/env bash
# Render packaging/homebrew/keel.rb for a real tag: substitute the version in
# the source `url` and the tarball's real sha256 (computed from the tag's
# GitHub-generated source archive, which only exists once the tag is pushed —
# so this is a release-workflow step, not something committed). The checked-in
# packaging/homebrew/keel.rb keeps a placeholder sha256 and stays
# syntax-checkable without a tag; this script never overwrites it, it writes
# to a separate output path.
#
#   scripts/render-homebrew-formula.sh vX.Y.Z <output-path>
#
# Downloads the tag's source tarball to compute the digest, so it only
# produces a correct result once `vX.Y.Z` exists on the `repository` remote
# (true when this runs from the release workflow, after the tag push that
# triggered it).
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

tag="${1:?usage: scripts/render-homebrew-formula.sh vX.Y.Z <output-path>}"
out="${2:?usage: scripts/render-homebrew-formula.sh vX.Y.Z <output-path>}"

if ! [[ "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z.-]+)?$ ]]; then
  echo "render-homebrew-formula: '$tag' is not a vX.Y.Z tag" >&2
  exit 1
fi
version="${tag#v}"

repo_url="$(python3 -c '
import tomllib
with open("Cargo.toml", "rb") as f:
    print(tomllib.load(f)["workspace"]["package"]["repository"])
')"
archive_url="$repo_url/archive/refs/tags/$tag.tar.gz"

workdir="$(mktemp -d "${TMPDIR:-/tmp}/keel-homebrew-render.XXXXXX")"
trap 'rm -rf "$workdir"' EXIT
tarball="$workdir/$tag.tar.gz"
echo "render-homebrew-formula: fetching $archive_url"
curl -fsSL "$archive_url" -o "$tarball"
sha256="$(shasum -a 256 "$tarball" | cut -d' ' -f1)"

mkdir -p "$(dirname "$out")"
# The url/sha256 patterns match any tag (not just the checked-in placeholder's
# v0.1.0), so this keeps working as scripts/bump-version.sh moves the version.
sed \
  -e "s#^  url \"[^\"]*/archive/refs/tags/[^\"]*\.tar\.gz\"\$#  url \"$archive_url\"#" \
  -e "s/^  sha256 \"[0-9a-f]\{64\}\"\$/  sha256 \"$sha256\"/" \
  packaging/homebrew/keel.rb >"$out"

# The formula reads `version` from the `url`'s own version detection, but
# assert we actually substituted both fields (a Formula-syntax drift in the
# source file would otherwise silently produce an unrendered copy).
grep -qF "$archive_url" "$out" || {
  echo "render-homebrew-formula: url substitution failed — check packaging/homebrew/keel.rb's url line" >&2
  exit 1
}
grep -qF "sha256 \"$sha256\"" "$out" || {
  echo "render-homebrew-formula: sha256 substitution failed — check packaging/homebrew/keel.rb's sha256 line" >&2
  exit 1
}
ruby -c "$out" >/dev/null

echo "render-homebrew-formula: wrote $out (version $version, sha256 $sha256)"
