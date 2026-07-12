#!/usr/bin/env python3
"""Async bridge test for the `keel_core` PyO3 module (sprint risk item #2).

Exercises `KeelCore.execute_async` end to end on a real asyncio loop with a
RETRYING scenario: an `async def` effect fails the first (idempotent) attempt
with a retryable `conn` error, the engine backs off on real time, and the second
attempt succeeds — proving the awaitable resolves with the outcome dict and the
async effect coroutine is awaited on the caller's loop.

Requires the module to be built and importable (`maturin develop` in
crates/keel-py). Run with the venv's Python:  python crates/keel-py/tests/test_async_bridge.py
Exit code 0 on success.
"""

from __future__ import annotations

import asyncio
import sys

import keel_core


def test_async_retry_round_trip() -> None:
    core = keel_core.KeelCore()  # non-paused: real (short) backoff
    core.configure(
        {
            "defaults": {
                "outbound": {
                    "timeout": "5s",
                    # fixed(1ms) keeps the real backoff tiny but nonzero.
                    "retry": {"attempts": 3, "schedule": "fixed(1ms)", "on": ["conn", "5xx"]},
                }
            }
        }
    )

    seen: list[int] = []

    async def effect(attempt: int) -> dict:
        seen.append(attempt)
        # Yield to the loop to prove we are genuinely awaited on it.
        await asyncio.sleep(0)
        if attempt == 1:
            return {"status": "error", "class": "conn", "message": "connection reset"}
        return {"status": "ok", "payload": {"attempt": attempt}}

    async def main() -> dict:
        request = {
            "v": 1,
            "target": "api.example.com",
            "op": "GET api.example.com/items",
            "idempotent": True,
            "args_hash": "h1",
        }
        return await core.execute_async(request, effect)

    outcome = asyncio.run(main())

    assert outcome["result"] == "ok", outcome
    assert outcome["attempts"] == 2, outcome
    assert outcome["payload"] == {"attempt": 2}, outcome
    assert outcome["from_cache"] is False, outcome
    assert outcome["waits_ms"] == [1], outcome
    assert seen == [1, 2], seen
    print(f"async bridge: retry round-trip OK -> {outcome}")


def test_async_non_idempotent_not_retried() -> None:
    """A non-idempotent failure must NOT be retried (KEEL-E014), even async."""
    core = keel_core.KeelCore()
    core.configure(
        {"defaults": {"outbound": {"retry": {"attempts": 3, "on": ["conn"]}}}}
    )

    calls = 0

    async def effect(attempt: int) -> dict:
        nonlocal calls
        calls += 1
        return {"status": "error", "class": "conn", "message": "reset"}

    async def main() -> dict:
        request = {"v": 1, "target": "api.example.com", "op": "POST x", "idempotent": False}
        return await core.execute_async(request, effect)

    outcome = asyncio.run(main())
    assert outcome["result"] == "error", outcome
    assert outcome["error"]["code"] == "KEEL-E014", outcome
    assert outcome["attempts"] == 1, outcome
    assert calls == 1, calls
    print(f"async bridge: non-idempotent-not-retried OK -> {outcome['error']['code']}")


def main() -> int:
    test_async_retry_round_trip()
    test_async_non_idempotent_not_retried()
    print("async bridge tests: 2/2 passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
