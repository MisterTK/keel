# Keel v0.3.0 — poll, subprocess flows, and a more honest doctor

## Headline

Two new zero-code-change capabilities: a `poll` primitive that collapses
hand-rolled poll-until-done loops into per-target policy, and `keel exec` —
one external command wrapped as a journaled durable flow with at-most-once
dispatch per identity. Around them, the doctor got materially more honest:
process-topology classification, ranked follow-ups, hand-rolled-pattern
detection, a stdlib `urllib.request` pack (projects with zero third-party
HTTP clients are no longer invisible), and code-hash staleness surfaced
before a resume instead of during one.

## What's new

Poll (KEEL-E016). `[target."api.example.com".poll]` with `interval`,
`deadline`, and `until = { field = "status", terminal = ["done", "failed"] }`
turns a submit-then-poll API into one call: each poll iteration is a fully
retried request; cache, rate limit, and breaker see the whole poll as a
single call; a deadline miss is KEEL-E016, breaker-countable. Applies to
idempotent GET/HEAD only; non-JSON responses and responses without the
configured field pass through unchanged (fail-open). Identical semantics in
the real core and all three Tier-1 stubs, pinned by seven new conformance
scenarios.

Subprocess flows (`keel exec`, KEEL-E033). `keel exec --flow nightly --
<command...>` runs one external command as a journaled Tier-2 flow:
at-most-once dispatch per identity, `[flows] on_busy = "skip"|"wait"|"fail"`
for concurrent invocations, dead-PID abandonment (a crashed holder releases
the lease), and a declared-side-effect gate — if a declared `--journal-file`
changed since a failed run began (line count + hash recorded per attempt),
the retry stops with KEEL-E033 unless `--force` is passed. Honesty note:
"at-most-once dispatch" is per journal identity and lease — it is not a
distributed lock, and Keel provides crash-safe retry gating for external
commands, not exactly-once execution inside an opaque child; the
side-effect gate exists precisely because Keel cannot know whether a dead
attempt's work is safe to repeat. Completed flows replay their recorded
outcome instead of respawning the child. The runtime-native form
(intercepting `subprocess.run`/`child_process.spawn` in-process) is a named
fast-follow, not in this release.

Code-hash staleness, surfaced early. `keel flows` / MCP `list_flows` now
report `code_hash` and tri-state `code_hash_stale` per flow, and `keel
doctor` emits a `code-hash-stale` follow-up when a resumable flow's recorded
hash no longer matches the code on disk ("resuming may replay against
changed code"). The resume fence itself already existed; it is now visible
before a resume, not during one. `cmd:`/`ts:` flows report `null` (their
current hash is not re-derivable from the journal alone in v1 — honesty
over coverage).

stdlib `urllib.request` pack. Python projects that never adopted requests
or httpx were invisible to Keel; now `OpenerDirector.open` (covering
`urlopen`, `build_opener`, `install_opener`, and held opener references) is
wrapped with the full policy stack, preserving urllib's raise semantics
(including cache-replayed 4xx re-raising `HTTPError`) and honoring
tighter-wins timeouts through `open`'s own timeout parameter. POST bodies
folded into a `Request` object are judged by effective method. Certified in
CI's adapter farm like every third-party pack.

Doctor honesty (topology, follow-ups, hand-rolled patterns). `keel doctor`
now classifies what it cannot wrap instead of staying silent: URL-transport
processes, subprocess invocations, and dependency-averse files each get a
bucket with a stated reason. Reports carry ranked `follow_ups` (a closed
five-code set, confidence-ascending) so agents and humans get the next
action, not just a wall of findings. The scanner detects hand-rolled retry
loops, hand-rolled poll loops, and silent exception swallows, and pairs each
finding with the policy that replaces it — `keel init --diff` annotates
which hand-rolled code becomes deletable once a target is wrapped.

## Error-code bookkeeping (CCR-3 / CCR-4)

The two contract change requests for this release were approved naming
KEEL-E040 (poll deadline) and KEEL-E041 (side effects recorded). Both
numbers collided with the frozen internal block (E040 has been `internal`
since contracts-v1), so the codes land as **KEEL-E016**
(poll-deadline-exceeded) and **KEEL-E033** (side-effects-recorded).
Semantics are exactly as approved; only the numbers moved. Dated amendments
are appended to the CCR documents. `keel explain KEEL-E016` /
`keel explain KEEL-E033` work.

## Toolchain

Rust toolchain pin moves 1.97.0 → 1.97.1 (`rust-version` floor stays 1.97);
tokio stays on the 1.47 LTS line.

## Supply-chain posture

Unchanged from v0.2.0 and stated plainly (see SECURITY.md): crates.io and
PyPI publishes are attested (PyPI via Trusted Publishing/OIDC); npm packages
are NOT yet published with `--provenance` — enabling it is a tracked
follow-up, and SECURITY.md is the source of truth for what is and is not
attested today.

## Known caveats

- `keelrun-cli-win32-x64` is still blocked by npm's spam-detection 403
  (issue #20; awaiting an npm support ticket). All other npm packages
  publish normally.
- One pre-existing test (`mcp_subprocess_transcript_matches_in_process`)
  is red since 2026-07-18 for clock reasons unrelated to this release.
