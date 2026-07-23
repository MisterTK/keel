#!/usr/bin/env python3
"""Conformance runner: interprets conformance/scenarios/*.json against a core
implementation (default: the Python keel-core-stub). See conformance/README.md
for the scenario format and the normative execution semantics.

Usage:
    python3 conformance/runner.py [--impl {stub,native}] [--scenarios DIR]

`--impl stub` (default) drives the pure-Python keel-core-stub; `--impl native`
drives the PyO3 `keel_core` module (build it first with `maturin develop` in
crates/keel-py). Both must pass 18/18 — the same scenarios and runner logic, only
the injected core object differs. Exit code 0 iff every scenario passes.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path
from typing import Any, Callable

ROOT = Path(__file__).resolve().parent.parent


def load_impl(name: str) -> tuple[Callable[[], Any], type[Exception]]:
    """Resolve `(make_core, error_cls)` for the chosen implementation.

    `make_core()` builds a fresh core on a virtual clock at 0 (stub natively;
    native via the harness-only `paused=True` flag), so the runner's
    configure/execute/report/advance_clock loop is identical for both.
    """
    if name in ("stub", "python-stub"):
        sys.path.insert(0, str(ROOT / "python" / "keel-core-stub"))
        from keel_core_stub import KeelCoreStub, KeelError

        return KeelCoreStub, KeelError
    if name == "native":
        try:
            import keel_core
        except ImportError as e:
            raise SystemExit(
                f"--impl native: cannot import keel_core ({e}); "
                "build it with `maturin develop` in crates/keel-py first"
            ) from e

        return (lambda: keel_core.KeelCore(paused=True)), keel_core.KeelCoreError
    raise SystemExit(f"unknown --impl {name!r} (available: stub, native)")


def subset_mismatches(actual: Any, expected: Any, path: str = "$") -> list[str]:
    """Subset match: dicts require listed keys to match recursively; lists
    must match exactly (element-wise subset); scalars must be equal."""
    if isinstance(expected, dict):
        if not isinstance(actual, dict):
            return [f"{path}: expected object, got {actual!r}"]
        out: list[str] = []
        for k, v in expected.items():
            if k not in actual:
                out.append(f"{path}.{k}: missing (expected {v!r})")
            else:
                out.extend(subset_mismatches(actual[k], v, f"{path}.{k}"))
        return out
    if isinstance(expected, list):
        if not isinstance(actual, list):
            return [f"{path}: expected array, got {actual!r}"]
        if len(actual) != len(expected):
            return [f"{path}: expected {expected!r}, got {actual!r}"]
        out = []
        for i, (a, e) in enumerate(zip(actual, expected)):
            out.extend(subset_mismatches(a, e, f"{path}[{i}]"))
        return out
    if isinstance(expected, bool) or isinstance(actual, bool):
        # bool is an int in Python; compare strictly so 1 != true
        return [] if actual is expected else [f"{path}: expected {expected!r}, got {actual!r}"]
    return [] if actual == expected else [f"{path}: expected {expected!r}, got {actual!r}"]


def run_scenario(scenario: dict[str, Any], make_core, error_cls) -> list[str]:
    if scenario.get("tier", 1) != 1:
        # Tier 2 (durable flows) is real-core only; the stub skips it cleanly.
        return []
    core = make_core()
    want_cfg_err = scenario.get("expect_configure_error")
    try:
        core.configure(scenario["policy"])
    except error_cls as e:
        if want_cfg_err:
            if e.code == want_cfg_err:
                return []
            return [f"configure: expected {want_cfg_err}, got {e.code}"]
        return [f"configure: unexpected error {e}"]
    if want_cfg_err:
        return [f"configure: expected {want_cfg_err}, but configure succeeded"]

    failures: list[str] = []
    for i, step in enumerate(scenario["steps"]):
        label = f"step[{i}]"
        if "advance_ms" in step:
            core.advance_clock(step["advance_ms"])
        elif "report_expect" in step:
            failures += [
                f"{label} report: {m}"
                for m in subset_mismatches(core.report(), step["report_expect"])
            ]
        elif "call" in step:
            call = step["call"]
            request = {"v": 1, "target": call["target"], "op": call["target"]}
            request.update(call.get("request", {}))
            script = call.get("effect", [])
            consumed = 0

            def effect(attempt: int) -> dict[str, Any]:
                nonlocal consumed
                if consumed >= len(script):
                    raise AssertionError(
                        f"{label}: effect script exhausted (attempt {attempt}, "
                        f"scripted {len(script)})"
                    )
                res = script[consumed]
                consumed += 1
                return res

            try:
                outcome = core.execute(request, effect)
            except AssertionError as e:
                failures.append(str(e))
                continue
            failures += [
                f"{label} outcome: {m}"
                for m in subset_mismatches(outcome, call.get("expect", {}))
            ]
            if consumed != len(script):
                failures.append(
                    f"{label}: effect script not fully consumed "
                    f"({consumed}/{len(script)} attempts used)"
                )
        elif "resolve" in step:
            r = step["resolve"]
            got = core.resolve_target(r["method"], r["host"], r.get("scheme"), r.get("port"), r.get("path"))
            if got != step["expect"]:
                failures.append(f"{label}: resolve got {got!r}, want {step['expect']!r}")
        elif "layer" in step:
            l = step["layer"]
            got = core.layer(l["target"], l["key"])
            if got != step["expect"]:
                failures.append(f"{label}: layer got {got!r}, want {step['expect']!r}")
        else:
            failures.append(f"{label}: unknown step {sorted(step)}")
    return failures


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--impl", default="stub", choices=["stub", "native", "python-stub"])
    ap.add_argument("--scenarios", default=str(ROOT / "conformance" / "scenarios"))
    args = ap.parse_args()

    make_core, error_cls = load_impl(args.impl)
    files = sorted(Path(args.scenarios).glob("*.json"))
    if not files:
        print(f"no scenarios found in {args.scenarios}", file=sys.stderr)
        return 2

    failed = 0
    skipped = 0
    for f in files:
        scenario = json.loads(f.read_text())
        if scenario.get("tier", 1) != 1:
            # Tier 2 (durable flows) is real-core only; the stub skips it.
            skipped += 1
            print(f"skip  {scenario['name']}  (tier {scenario['tier']})")
            continue
        mismatches = run_scenario(scenario, make_core, error_cls)
        if mismatches:
            failed += 1
            print(f"FAIL  {scenario['name']}  ({f.name})")
            for m in mismatches:
                print(f"      {m}")
        else:
            print(f"ok    {scenario['name']}")
    total = len(files) - skipped
    suffix = f" ({skipped} tier-2 skipped)" if skipped else ""
    print(f"\n{total - failed}/{total} scenarios passed  [impl: {args.impl}]{suffix}")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
