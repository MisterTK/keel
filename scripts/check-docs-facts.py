#!/usr/bin/env python3
"""Assert the countable claims in Keel's doc surfaces match the code.

Why this exists: four doc surfaces have gone stale in three consecutive
programs — `llms.txt`, `llms-full.txt`, the Claude Code skill, and the
`agents-snippet.md` template Keel writes into an adopter's AGENTS.md. Each was
found by accident, never by a check. The per-slice discipline reliably keeps
README and the code current because those are what the work touches; nothing
routes to the rest.

This does NOT try to verify that documentation is accurate in general — it
cannot. It pins the small set of facts that are DERIVABLE FROM THE REPO, which
is exactly the set that actually drifted:

  - the conformance scenario counts (58 / 45 Tier 1 / 13 Tier 2)
  - the number of `KEEL_LOG_FORMAT=json` line kinds, and that Python and Node
    agree on it
  - the error-code set in `llms-full.txt`, which claims to be exhaustive
  - `llms.txt`'s version stamp, swept by neither bump-version.sh nor
    check-versions.py

A fact only belongs here if a wrong value is checkable without judgment. Prose
accuracy stays a human problem; these numbers no longer are.

Usage: check-docs-facts.py
Exit 0 when every claim agrees, 1 with one line per mismatch. Stdlib-only;
deterministic output.
"""

from __future__ import annotations

import json
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent

# Surfaces that state a JSON-line-kind count. README is included even though
# the per-slice discipline usually keeps it current: it was wrong this time.
KIND_COUNT_SURFACES = (
    "README.md",
    "llms.txt",
    "llms-full.txt",
    "skills/keel/SKILL.md",
    "packaging/claude-skill/keel/SKILL.md",
    "crates/keel-cli/templates/agents-snippet.md",
)

NUMBER_WORDS = {
    "two": 2,
    "three": 3,
    "four": 4,
    "five": 5,
    "six": 6,
    "seven": 7,
    "eight": 8,
    "nine": 9,
    "ten": 10,
}


def fail(problems: list[str], msg: str) -> None:
    problems.append(msg)


def scenario_counts() -> tuple[int, int, int]:
    """(total, tier1, tier2) counted off disk, not off a doc."""
    files = sorted((ROOT / "conformance" / "scenarios").glob("*.json"))
    tier2 = 0
    for f in files:
        try:
            doc = json.loads(f.read_text())
        except json.JSONDecodeError as exc:  # a malformed scenario is its own bug
            raise SystemExit(f"check-docs-facts: {f.name} is not valid JSON: {exc}")
        if doc.get("tier") == 2:
            tier2 += 1
    return len(files), len(files) - tier2, tier2


def json_line_kinds() -> tuple[int, int]:
    """Count `severity` assignment sites per language.

    Each `KEEL_LOG_FORMAT=json` object kind assigns `severity` exactly once at
    its construction site, so counting those sites counts the kinds. The two
    languages are required to agree — that parity IS the contract, and a
    divergence here is the defect class this repo cares most about.
    """
    py_files = sorted((ROOT / "python" / "keel" / "src" / "keel").rglob("*.py"))
    node_files = sorted((ROOT / "node" / "keel").rglob("*.mjs"))
    node_files = [f for f in node_files if "vendor" not in f.parts and "test" not in f.parts]

    py_pat = re.compile(r'"severity"\s*(?::|\]\s*=)\s*"(INFO|WARNING|ERROR)"')
    node_pat = re.compile(r'severity\s*:\s*"(INFO|WARNING|ERROR)"')

    py = sum(len(py_pat.findall(f.read_text())) for f in py_files)
    node = sum(len(node_pat.findall(f.read_text())) for f in node_files)
    return py, node


def error_codes() -> set[str]:
    doc = json.loads((ROOT / "contracts" / "error-codes.json").read_text())
    codes = doc["codes"]
    if isinstance(codes, dict):
        return set(codes.keys())
    return {c["code"] for c in codes}


def workspace_version() -> str:
    text = (ROOT / "Cargo.toml").read_text()
    m = re.search(r'^\s*version\s*=\s*"([^"]+)"', text, re.M)
    if not m:
        raise SystemExit("check-docs-facts: no [workspace.package] version in Cargo.toml")
    return m.group(1)


def stated_counts(text: str, noun_pat: str) -> set[int]:
    """Every count stated immediately before `noun_pat`, digits or number-words."""
    found: set[int] = set()
    for m in re.finditer(rf"\b(\w+)\s+{noun_pat}", text, re.I):
        tok = m.group(1).lower()
        if tok.isdigit():
            found.add(int(tok))
        elif tok in NUMBER_WORDS:
            found.add(NUMBER_WORDS[tok])
    return found


def main() -> int:
    problems: list[str] = []

    # --- conformance scenario counts -------------------------------------
    total, tier1, tier2 = scenario_counts()
    full = (ROOT / "llms-full.txt").read_text()
    m = re.search(r"(\d+)\s+scenarios in `conformance/scenarios/`\s*\((\d+)\s*Tier 1,\s*(\d+)\s*Tier 2\)", full)
    if not m:
        fail(problems, "llms-full.txt: no 'N scenarios in `conformance/scenarios/` (N Tier 1, N Tier 2)' claim found")
    else:
        claimed = (int(m.group(1)), int(m.group(2)), int(m.group(3)))
        if claimed != (total, tier1, tier2):
            fail(
                problems,
                f"llms-full.txt claims {claimed[0]} scenarios / {claimed[1]} Tier 1 / {claimed[2]} Tier 2; "
                f"conformance/scenarios/ holds {total} / {tier1} / {tier2}",
            )

    # --- KEEL_LOG_FORMAT=json line kinds ---------------------------------
    py_kinds, node_kinds = json_line_kinds()
    if py_kinds != node_kinds:
        fail(
            problems,
            f"severity assignment sites diverge: Python {py_kinds}, Node {node_kinds} — "
            "the two front ends must emit the same JSON line kinds",
        )
    kinds = py_kinds
    if kinds == 0:
        fail(problems, "found no severity assignment sites at all; the detector is broken, not the docs")
    else:
        for rel in KIND_COUNT_SURFACES:
            path = ROOT / rel
            if not path.exists():
                fail(problems, f"{rel}: missing (listed as a doc surface that states the kind count)")
                continue
            text = path.read_text()
            stated = stated_counts(text, r"(?:lines?|kinds?)\b")
            wrong = {n for n in stated if n != kinds and 2 <= n <= 10}
            # Only flag a surface that talks about the JSON log format at all.
            if "KEEL_LOG_FORMAT" in text and wrong:
                fail(
                    problems,
                    f"{rel} states {sorted(wrong)} JSON log line kind(s) near 'lines'/'kinds'; "
                    f"the code emits {kinds}",
                )

    # --- error codes: llms-full.txt claims to be exhaustive --------------
    real = error_codes()
    named = set(re.findall(r"KEEL-E\d{3}", full))
    missing = sorted(real - named)
    if missing:
        fail(problems, f"llms-full.txt omits error code(s) {', '.join(missing)}")
    unknown = sorted(named - real)
    if unknown:
        fail(problems, f"llms-full.txt names non-existent error code(s) {', '.join(unknown)}")

    # --- llms.txt version stamp ------------------------------------------
    version = workspace_version()
    llms = (ROOT / "llms.txt").read_text()
    stamp = re.search(r"^Status \(([^)]*)", llms, re.M)
    if not stamp:
        fail(problems, "llms.txt: no 'Status (...)' line found")
    else:
        # The HEADLINE version — the first one named — must be the workspace
        # version, not merely mentioned somewhere in the stamp. Checking only
        # for presence is too weak: right after a bump, a stamp reading
        # "v0.6.5 is the newest release; main carries unreleased 0.7.0 work"
        # contains the new version while asserting the opposite of the truth.
        first = re.search(r"v?(\d+\.\d+\.\d+)", stamp.group(1))
        if not first:
            fail(problems, f"llms.txt's status line names no version: {stamp.group(1)!r}")
        elif first.group(1) != version:
            fail(
                problems,
                f"llms.txt's status line leads with {first.group(1)}, but the workspace version is "
                f"{version} — bump-version.sh does not sweep this, so it must be edited by hand",
            )

    if problems:
        for p in problems:
            print(f"check-docs-facts: {p}")
        print(f"check-docs-facts: FAILED ({len(problems)} problem(s))")
        return 1

    print(
        f"check-docs-facts: OK — {total} scenarios ({tier1} Tier 1, {tier2} Tier 2), "
        f"{kinds} JSON line kinds (py == node), {len(real)} error codes, stamp names {version}"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
