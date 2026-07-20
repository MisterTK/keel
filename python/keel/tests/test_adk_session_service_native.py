"""Design doc issue #15 §6 item 6 / §3.2's own flagged open question: are
the new `flows_by_entrypoint`/`steps_for_flow` `Journal` read methods
(§3.2, `KeelSessionService`'s read path) safe to call from INSIDE an
already-open Tier 2 flow — the same shape a `get_session`/`list_sessions`
call made from a tool implementation mid-turn would exercise?

Issue #38 documents a CONFIRMED native-core re-entrancy deadlock between two
different call sites (`adk_pack`'s `tool:` wrap and `mcp_pack`'s `mcp:` wrap)
both re-entering the core's open-flow bookkeeping via `execute_async` for the
same outgoing call while a flow is open. This design's own §3.2 argues the
NEW read methods are safe on lock-independence grounds (`Engine::journal()`
is a path entirely separate from `active_flow: Arc<AsyncMutex<Option
<FlowHandle>>>` — confirmed during design review by tracing both fields'
usage) but explicitly states that argument has NOT been checked against
issue #38's specific failure shape, and requires "a minimal repro (as #38's
own suggested next step describes) before implementation."

This module IS that minimal repro: a native-core-only reproduction with NO
ADK and NO MCP involved (matching #38's own "Suggested next step" verbatim:
"a minimal native-core repro without ADK/MCP... two call sites re-entering
... while a flow is open") — open a real Tier 2 flow via the real native
core, then from INSIDE that open flow synchronously call
`backend.flows_by_entrypoint(...)` and `backend.steps_for_flow(...)`
directly. Each call is wrapped in a hard wall-clock timeout (a background
thread + `.join(timeout=...)`) so a REAL deadlock fails this test fast and
loud instead of hanging CI forever.

Mirrors `test_adk_runner_flows_native.py`'s `_NativeAdkFlowTestBase`
conventions (same native-backend setup shape) — reused directly via import
rather than reimplemented, per that module's own docstring: "no fake
backend anywhere in this module."

Requires the native core (`keel_core`); skips cleanly without it, same as
`test_adk_runner_flows_native.py`.
"""

from __future__ import annotations

import threading
import unittest
from typing import Any, Callable

from tests.test_adk_runner_flows_native import _NATIVE, _NativeAdkFlowTestBase


class _Result:
    """A tiny box a background thread writes its outcome into — read from
    the main thread only AFTER `.join()` returns (never while the thread
    might still be running), so there is no data race on the box itself."""

    def __init__(self) -> None:
        self.value: Any = None
        self.error: BaseException | None = None


def _call_with_hard_timeout(fn: Callable[[], Any], *, timeout_s: float = 5.0) -> _Result:
    """Runs `fn()` on a background thread and waits up to `timeout_s` wall-clock
    seconds for it to finish. A REAL native-core deadlock hangs the background
    thread forever; `.join(timeout=...)` returning while that thread is STILL
    ALIVE is exactly what must fail this test — loudly, fast, and without ever
    hanging the test runner itself (the thread is daemonized so a genuine
    deadlock does not also wedge process exit)."""
    box = _Result()

    def run() -> None:
        try:
            box.value = fn()
        except BaseException as exc:  # noqa: BLE001 - captured and re-raised on the main thread
            box.error = exc

    thread = threading.Thread(target=run, daemon=True)
    thread.start()
    thread.join(timeout=timeout_s)
    if thread.is_alive():
        raise AssertionError(
            f"native core call did not return within {timeout_s}s — this IS a deadlock "
            "(issue #38's failure shape: a call re-entering the native core's open-flow "
            "bookkeeping while a flow is open never returns). The thread is left running "
            "(daemonized) so this test process can still exit."
        )
    return box


@unittest.skipUnless(_NATIVE, "keel_core native module not built (maturin develop in crates/keel-py)")
class ReadMethodsInsideOpenFlowReentrancyTest(_NativeAdkFlowTestBase):
    """Issue #38's own suggested minimal repro, exactly: open one Tier 2 flow
    via the real native core (no ADK, no MCP anywhere in this class), then
    call the two new read-only `Journal` methods from INSIDE that open flow,
    on the SAME thread that holds it open — the precise shape a
    `KeelSessionService.get_session`/`list_sessions` call made from a tool
    implementation mid-turn would exercise (design §3.2's flagged concern)."""

    ENTRYPOINT = "py:issue38repro:probe"

    def test_flows_by_entrypoint_returns_promptly_from_inside_an_open_flow(self) -> None:
        backend = self.native_backend()
        info = backend.enter_flow(self.ENTRYPOINT, "ah-38-a", code_hash=None, explicit_key=None, lease_ms=None)
        try:
            result = _call_with_hard_timeout(lambda: backend.flows_by_entrypoint(self.ENTRYPOINT))
        finally:
            backend.exit_flow("completed")
        self.assertIsNone(result.error, f"flows_by_entrypoint raised from inside an open flow: {result.error!r}")
        flow_ids = {f["flow_id"] for f in result.value}
        self.assertIn(info["flow_id"], flow_ids)

    def test_steps_for_flow_returns_promptly_from_inside_an_open_flow(self) -> None:
        backend = self.native_backend()
        info = backend.enter_flow(self.ENTRYPOINT, "ah-38-b", code_hash=None, explicit_key=None, lease_ms=None)
        try:
            result = _call_with_hard_timeout(lambda: backend.steps_for_flow(info["flow_id"]))
        finally:
            backend.exit_flow("completed")
        self.assertIsNone(result.error, f"steps_for_flow raised from inside an open flow: {result.error!r}")
        self.assertIsInstance(result.value, list)

    def test_both_read_methods_back_to_back_from_inside_the_same_open_flow(self) -> None:
        # The exact shape KeelSessionService's OWN `_scan_flows` (§3.2) uses:
        # one `flows_by_entrypoint` call, then a `steps_for_flow` call per
        # flow found — both from inside an open flow, back to back, on the
        # SAME thread holding it open.
        backend = self.native_backend()
        info = backend.enter_flow(self.ENTRYPOINT, "ah-38-c", code_hash=None, explicit_key=None, lease_ms=None)
        try:
            flows_result = _call_with_hard_timeout(lambda: backend.flows_by_entrypoint(self.ENTRYPOINT))
            self.assertIsNone(flows_result.error)
            steps_result = _call_with_hard_timeout(lambda: backend.steps_for_flow(info["flow_id"]))
            self.assertIsNone(steps_result.error)
        finally:
            backend.exit_flow("completed")
        self.assertIsInstance(flows_result.value, list)
        self.assertIsInstance(steps_result.value, list)


if __name__ == "__main__":
    unittest.main()
