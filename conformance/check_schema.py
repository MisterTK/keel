#!/usr/bin/env python3
"""Contract self-consistency check: every policy document in the repo must
validate against contracts/policy.schema.json — the smart-defaults pack and
each conformance scenario's policy (scenarios that expect a configure error
must fail validation, proving the schema catches what the stubs catch).

Requires: jsonschema (pip install jsonschema). Python 3.11+ (tomllib).
"""

from __future__ import annotations

import json
import sys
import tomllib
from pathlib import Path

import jsonschema

ROOT = Path(__file__).resolve().parent.parent
SCHEMA = json.loads((ROOT / "contracts" / "policy.schema.json").read_text())


def main() -> int:
    validator = jsonschema.Draft202012Validator(SCHEMA)
    failures: list[str] = []

    defaults = tomllib.loads((ROOT / "contracts" / "defaults.toml").read_text())
    errors = list(validator.iter_errors(defaults))
    if errors:
        failures += [f"defaults.toml: {e.json_path}: {e.message}" for e in errors]
    else:
        print("ok    contracts/defaults.toml")

    for f in sorted((ROOT / "conformance" / "scenarios").glob("*.json")):
        scenario = json.loads(f.read_text())
        errors = list(validator.iter_errors(scenario["policy"]))
        expect_invalid = "expect_configure_error" in scenario
        if expect_invalid and not errors:
            failures.append(
                f"{f.name}: policy expects a configure error but validates cleanly"
            )
        elif not expect_invalid and errors:
            failures += [f"{f.name}: {e.json_path}: {e.message}" for e in errors]
        else:
            note = "  (invalid, as designed)" if expect_invalid else ""
            print(f"ok    {f.name}{note}")

        # Tier-2 (flow) scenarios may reconfigure the engine mid-scenario via a
        # per-run policy override (e.g. to prove a replayed step ignores a
        # changed retry policy) — each override must validate too. Unlike the
        # top-level policy, no run override is ever expected to be invalid.
        for i, run in enumerate(scenario.get("runs", [])):
            if "policy" not in run:
                continue
            run_errors = list(validator.iter_errors(run["policy"]))
            if run_errors:
                failures += [
                    f"{f.name}: runs[{i}].policy: {e.json_path}: {e.message}"
                    for e in run_errors
                ]
            else:
                print(f"ok    {f.name} runs[{i}].policy")

    for m in failures:
        print(f"FAIL  {m}", file=sys.stderr)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
