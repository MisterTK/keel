#!/usr/bin/env python3
"""Dedicated micro-bench for the `resolve_target`/`layer` FFI crossing
(issue #50): `scripts/bench-overhead.sh`'s `overhead` criterion bench
measures `Engine::execute`'s in-process path only (per its own docstring,
"the same shape `keel-ffi`'s `keel_execute` drives") — it never crosses into
PyO3's actual method-dispatch/argument-marshalling path, which now runs once
per outbound HTTP call (SP-1's `_runtime.get_backend().resolve_target(...)`/
`.layer(...)`, called from every HTTP pack's `_judge`). This script drives
the REAL native-module call path a Python caller uses (`import keel_core;
keel_core.KeelCore().resolve_target(...)`), so what's measured is the actual
FFI-crossing cost, not an inference from a different (in-process) benchmark.

Skips gracefully — `native_loaded: false`, no measurement — when the native
wheel isn't built locally (`maturin develop -m crates/keel-py/Cargo.toml`),
mirroring `node/keel/scripts/measure-startup.mjs`'s own native-addon check
and `python/keel/tests/test_run.py`'s `StartupBudgetTest`.

Methodology mirrors `crates/keel-core/benches/support.rs`'s `median_ns`
exactly: 101 samples of 256-call batches, median of the batch means (robust
to scheduler blips, deterministic enough for a published number).

Two ways to use this module:
  - run directly (`python3 measure_resolve_target_ffi.py [--json <path>]`)
    for a local/manual measurement, printing a `[resolve_target ffi]` line
    and optionally emitting a JSON artifact (mirrors
    `scripts/bench-overhead.sh`'s two-phase pattern: the same measurement,
    one emits JSON);
  - imported (`measure()`) by anything that wants the raw result dict.
"""

from __future__ import annotations

import json
import sys
import time
from pathlib import Path
from typing import Any, Callable

INNER = 256
SAMPLES = 101


def median_ns(op: Callable[[], Any]) -> int:
    """Median nanoseconds per invocation of `op`, over `SAMPLES` batches of
    `INNER` calls each — the same median-of-batch-means estimator as the
    Rust bench's `median_ns`, so the two languages' numbers are directly
    comparable."""
    for _ in range(INNER):
        op()
    samples: list[int] = []
    for _ in range(SAMPLES):
        start = time.perf_counter_ns()
        for _ in range(INNER):
            op()
        samples.append((time.perf_counter_ns() - start) // INNER)
    samples.sort()
    return samples[SAMPLES // 2]


_KEYS = (
    "native_loaded",
    "resolve_target_llm_host_ns",
    "resolve_target_pattern_ns",
    "layer_null_ns",
    "layer_populated_ns",
)


def measure() -> dict[str, Any]:
    """`{native_loaded, resolve_target_llm_host_ns, resolve_target_pattern_ns,
    layer_null_ns, layer_populated_ns}` — all four `None` when the native
    wheel isn't built (nothing to measure). Two distinct shapes per method,
    not one, because a single "LLM host, unconfigured engine" case would
    only ever exercise resolve_target's cheapest branch (tier 1's exact
    host-map hit short-circuits before tier 3's pattern-collection/sort ever
    runs) and layer's cheapest return (three straight `None` misses to
    `Value::Null`) — neither is representative of a typical non-LLM call
    against a policy with configured `[target]` patterns, which is the
    common case every generic HTTP pack's `_judge` actually hits.

      - `*_llm_host_ns`: an unconfigured engine, host `api.openai.com` — the
        tier-1 LLM-host-map short-circuit; `layer` reads straight to `null`.
      - `*_pattern_ns`: an engine `configure`d with a `[target."*.example.
        com"]` pattern (a non-LLM host), resolving `api.example.com` — must
        fall through tiers 1/2 and run tier 3's sort/glob-match; `layer`
        reads the matched key's own populated `retry` config, a real nested
        value crossing the FFI boundary rather than a trivial `null`.
    """
    try:
        import keel_core
    except ImportError:
        return {"native_loaded": False, **{k: None for k in _KEYS if k != "native_loaded"}}

    bare = keel_core.KeelCore()
    resolve_target_llm_host_ns = median_ns(lambda: bare.resolve_target("GET", "api.openai.com"))
    layer_null_ns = median_ns(lambda: bare.layer("llm:openai", "retry"))

    configured = keel_core.KeelCore()
    configured.configure({"target": {"*.example.com": {"retry": {"attempts": 3}}}})
    resolve_target_pattern_ns = median_ns(lambda: configured.resolve_target("GET", "api.example.com"))
    layer_populated_ns = median_ns(lambda: configured.layer("*.example.com", "retry"))

    return {
        "native_loaded": True,
        "resolve_target_llm_host_ns": resolve_target_llm_host_ns,
        "resolve_target_pattern_ns": resolve_target_pattern_ns,
        "layer_null_ns": layer_null_ns,
        "layer_populated_ns": layer_populated_ns,
    }


def format_summary(result: dict[str, Any]) -> str:
    if not result["native_loaded"]:
        return "[resolve_target ffi] python native (skipped: no wheel)"
    return (
        f"[resolve_target ffi] python resolve_target llm_host={result['resolve_target_llm_host_ns']}ns "
        f"pattern={result['resolve_target_pattern_ns']}ns | "
        f"layer null={result['layer_null_ns']}ns populated={result['layer_populated_ns']}ns"
    )


def main() -> None:
    args = sys.argv[1:]
    json_path = args[args.index("--json") + 1] if "--json" in args else None
    result = measure()
    print(format_summary(result), file=sys.stderr)
    if json_path:
        Path(json_path).write_text(json.dumps(result, sort_keys=True, indent=2) + "\n")
        print(f"measure-resolve-target-ffi: artifact at {json_path}", file=sys.stderr)


if __name__ == "__main__":
    main()
