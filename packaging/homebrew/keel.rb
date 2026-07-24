# frozen_string_literal: true

# Homebrew formula DRAFT for the bare `keel` CLI (dx-spec §6: "brew install
# keel for the bare CLI"; sprint-plan's release-candidate line: "brew formula
# draft"). This satisfies the "draft" bar with a real, syntax-checked,
# source-build formula; it is NOT submitted to homebrew-core (the name `keel`
# is free there only because we would need our own tap — see
# docs/naming-decision.md, which is unaffected by this: tap formula names are
# namespaced per-tap, so `keel` is ours to use in `MisterTK/homebrew-keel`
# regardless of the PyPI/npm/crates.io outcome).
#
# The `url`/`sha256` below are placeholders for a real tagged release; the
# release workflow renders a copy with both filled in from the actual tag
# (see scripts/render-homebrew-formula.sh) and attaches it to the draft
# GitHub Release. This checked-in copy is what stays syntax-checked in CI
# without needing a real tag to exist yet.
#
# Local verification:
#   ruby -c packaging/homebrew/keel.rb            # syntax only, no formula DSL needed
#   brew style packaging/homebrew/keel.rb          # full lint, if brew is present
#   brew install --build-from-source packaging/homebrew/keel.rb   # after a real tag exists
require "json"

# The `keel` CLI (run | init | doctor | status | explain | flows | trace),
# built from source via cargo. See file header for the draft/publish status.
class Keel < Formula
  desc "SQLite of durable execution: resilience as a library, zero code changes"
  homepage "https://github.com/MisterTK/keel"
  url "https://github.com/MisterTK/keel/archive/refs/tags/v0.4.1.tar.gz"
  # Placeholder — the release workflow substitutes the real tarball digest
  # (`shasum -a 256`) for the tag being released. See
  # scripts/render-homebrew-formula.sh.
  sha256 "0000000000000000000000000000000000000000000000000000000000000000"
  license "Apache-2.0"
  head "https://github.com/MisterTK/keel.git", branch: "main"

  depends_on "rust" => :build

  def install
    # keel-cli is a workspace member; std_cargo_args scopes the install to it
    # (and its bin `keel`) without pulling in keel-py/keel-node's Python/Node
    # toolchain requirements — the CLI itself has none (NFR5: static binary,
    # no runtime dependencies).
    system "cargo", "install", *std_cargo_args(path: "crates/keel-cli")
  end

  test do
    assert_match "keel #{version}", shell_output("#{bin}/keel --version")
    report = JSON.parse(shell_output("#{bin}/keel explain KEEL-E011 --json"))
    assert_equal "KEEL-E011", report["code"]
    assert_equal "timeout", report["name"]
  end
end
