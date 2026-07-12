"""Level 0 embedded smart-defaults pack (DX spec §1).

The policy applied when no keel.toml is present. It MIRRORS
contracts/defaults.toml verbatim (that file is the frozen source of truth);
we embed rather than read it so the installed package is self-contained
(stdlib-only, works offline). Drift against the contract is caught by the
parity test, which parses contracts/defaults.toml with `tomllib` and asserts
deep equality when the repo file is present.

The Level 0 hard rules — never change success-path semantics; never retry
non-idempotent calls; do nothing if a call can't be wrapped safely — are
BEHAVIOR, not config, and are enforced in the front end / backend, not here.
"""

from __future__ import annotations

from typing import Any


def level0_defaults() -> dict[str, Any]:
    """The embedded Level 0 policy, as the dict the backend's `configure`
    expects (identical shape to keel.toml parsed to JSON)."""
    return {
        "defaults": {
            "outbound": {
                "timeout": "30s",
                "retry": {
                    "attempts": 3,
                    "schedule": "exp(200ms, x2, max 30s, jitter)",
                    "on": ["conn", "timeout", "429", "5xx"],
                },
                "breaker": {"failures": 5, "cooldown": "15s"},
            },
            "llm": {
                "timeout": "120s",
                "retry": {
                    "attempts": 6,
                    "schedule": "exp(500ms, x2, max 60s, jitter)",
                    "on": ["conn", "timeout", "429", "5xx"],
                },
                "breaker": {"failures": 5, "cooldown": "30s"},
                "cache": {"mode": "dev"},
            },
        }
    }
