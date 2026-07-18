# Security & Supply-Chain Posture

## Reporting a vulnerability

Open a GitHub security advisory on this repository (Security → Advisories →
Report a vulnerability), or file an issue asking for a private channel if
the report is sensitive. Do not include exploit details in a public issue.

## Artifact provenance (as of v0.3.0)

Keel ships through three registries; their provenance guarantees differ and
we state them honestly rather than uniformly:

- **PyPI** (`keelrun`, `keelrun-cli`, `keelrun-core`): published via OIDC
  Trusted Publishing from GitHub Actions (`release.yml`; `id-token: write`,
  no long-lived tokens). PyPI records the publishing workflow identity.
- **npm** (`keelrun`, `keelrun-cli`, per-platform binaries): published from
  GitHub Actions with a registry token. npm provenance attestations
  (`npm publish --provenance`) are **not yet enabled** — tracked as a
  release-infrastructure follow-up.
- **crates.io** (6 crates): published manually from a maintainer machine
  with a local token, in dependency order. No provenance attestation exists
  for crates.io artifacts beyond crates.io's own publish log.

## MCP server posture

`keel mcp` is a local, client-launched stdio server: it owns no port, runs
no daemon, and exits on stdin EOF. Its tool outputs are deterministic
(sorted keys, no timestamps) and byte-identical to the corresponding CLI
`--json` commands, so they can be diffed and audited. Doctor/init reports
interpolate only hostnames, file paths, library names, and literal
subprocess argv from the scanned project — never raw source lines
(CI-enforced). Policy proposals are returned as diffs and are never written
without an explicit apply step by the caller.
