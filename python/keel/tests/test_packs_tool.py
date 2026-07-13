"""The ``tool:`` semantic target pack (dx-spec §4.1) and its wrap API.

The load-bearing judgment is Level 0's hard rule: a tool call is NON-idempotent
by default — observed, never retried (KEEL-E014) — with retry an explicit
wrap-site opt-in (``idempotent=True``), so these tests run against the real
stub backend (virtual clock — no real sleeps) and a real SQLite discovery
store. Defaults inheritance is asserted against the composed Level 0 policy
(``apply_pack_defaults``): with no ``[defaults.tool]`` in the frozen pack, a
``tool:`` target inherits ``[defaults.outbound]``.
"""

from __future__ import annotations

import asyncio
import sqlite3
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory
from typing import Any

from keel import _runtime
from keel._backend import load_backend
from keel._defaults import apply_pack_defaults
from keel._discovery import Discovery
from keel._errors import KeelError
from keel._wrap import WRAPPED_ATTR
from keel.packs.tool import (
    TARGET_ATTR,
    classify_tool_error,
    is_valid_tool_name,
    tool_pack,
    tool_target,
    wrap_tool,
)

_RETRY_CONN = {"attempts": 3, "on": ["conn"], "schedule": "fixed(1ms)"}


class ToolTestBase(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = TemporaryDirectory()
        self.cwd = Path(self._tmp.name)
        self._discoveries: list[Discovery] = []

    def tearDown(self) -> None:
        _runtime.clear_runtime()
        for d in self._discoveries:  # close SQLite connections (no ResourceWarning)
            d.close()
        self._tmp.cleanup()

    def install(self, policy: dict[str, Any]) -> tuple[Any, Discovery]:
        backend = load_backend("stub")
        backend.configure(policy)
        discovery = Discovery(self.cwd)
        self._discoveries.append(discovery)
        _runtime.set_runtime(backend, discovery)
        return backend, discovery

    def read_rows(self, discovery: Discovery) -> dict[str, sqlite3.Row]:
        discovery.close()  # flush WAL and release before we read
        conn = sqlite3.connect(discovery.db_path)
        conn.row_factory = sqlite3.Row
        try:
            return {r["target"]: r for r in conn.execute("SELECT * FROM discovery")}
        finally:
            conn.close()


class NameAndClassificationTest(unittest.TestCase):
    def test_tool_name_grammar_matches_frozen_target_key(self) -> None:
        # The <name> part of policy.schema.json's targetKey for tool: targets.
        for name in ("web_search", "a", "0day", "_x", "a.b-c_d", "Search2"):
            self.assertTrue(is_valid_tool_name(name), name)
            self.assertEqual(tool_target(name), f"tool:{name}")
        for name in ("", "get weather", "-lead", ".dot", "a/b", "a:b", "täl", None, 7):
            self.assertFalse(is_valid_tool_name(name), repr(name))

    def test_invalid_name_raises_keel_e001(self) -> None:
        with self.assertRaises(KeelError) as ctx:
            wrap_tool("get weather", lambda: None)
        self.assertEqual(ctx.exception.code, "KEEL-E001")

    def test_non_callable_raises_keel_e001(self) -> None:
        with self.assertRaises(KeelError) as ctx:
            wrap_tool("ok_name", "not callable")  # type: ignore[arg-type]
        self.assertEqual(ctx.exception.code, "KEEL-E001")

    def test_classify_timeout_conn_other(self) -> None:
        self.assertEqual(classify_tool_error(TimeoutError("slow")), "timeout")
        self.assertEqual(classify_tool_error(asyncio.TimeoutError()), "timeout")
        self.assertEqual(classify_tool_error(ConnectionError("down")), "conn")
        self.assertEqual(classify_tool_error(ConnectionResetError("reset")), "conn")
        self.assertEqual(classify_tool_error(ValueError("bad args")), "other")
        self.assertEqual(classify_tool_error(OSError("disk")), "other")


class ToolPackContractTest(unittest.TestCase):
    def test_four_operations(self) -> None:
        det = tool_pack.detect()
        self.assertTrue(det.matched)
        self.assertEqual(det.name, "tool")
        self.assertEqual(det.confidence, "pinned")
        self.assertEqual(tool_pack.seams(), [])  # the seam belongs to the framework pack
        decls = tool_pack.targets()
        self.assertEqual(decls[0].pattern, "tool:<name>")
        self.assertEqual(decls[0].kind, "tool")
        self.assertIn("non-idempotent by default", decls[0].idempotency_rule)
        # No [defaults.tool] in the frozen pack: tool: inherits [defaults.outbound].
        self.assertEqual(tool_pack.defaults(), {})


class NonIdempotentDefaultTest(ToolTestBase):
    def test_conn_error_observed_not_retried_e014(self) -> None:
        # Even with retry-on-conn configured on the exact target, the default
        # (non-idempotent) judgment blocks the retry: one attempt, KEEL-E014.
        backend, _ = self.install({"target": {"tool:charge_card": {"retry": _RETRY_CONN}}})
        calls = {"n": 0}
        original = ConnectionError("connection reset")

        def charge() -> None:
            calls["n"] += 1
            raise original

        wrapped = wrap_tool("charge_card", charge)
        with self.assertRaises(ConnectionError) as ctx:
            wrapped()
        self.assertIs(ctx.exception, original, "original exception object re-raised")
        self.assertEqual(calls["n"], 1, "a side-effecting tool is NOT retried")
        self.assertEqual(ctx.exception.keel_outcome["error"]["code"], "KEEL-E014")

    def test_tool_bug_propagates_unchanged_e015(self) -> None:
        # Class `other` is not in the on-list: non-retryable → E015 on attempt 1.
        self.install({"target": {"tool:charge_card": {"retry": _RETRY_CONN}}})
        original = ValueError("bad tool args")

        def charge() -> None:
            raise original

        with self.assertRaises(ValueError) as ctx:
            wrap_tool("charge_card", charge)()
        self.assertIs(ctx.exception, original)
        self.assertEqual(ctx.exception.keel_outcome["error"]["code"], "KEEL-E015")

    def test_never_cached_even_with_cache_configured(self) -> None:
        # args_hash is None for a non-idempotent tool, so a (misconfigured)
        # cache layer on the target can never serve it.
        self.install({"target": {"tool:send_mail": {"cache": {"ttl": "60s"}}}})
        calls = {"n": 0}

        def send() -> str:
            calls["n"] += 1
            return f"sent-{calls['n']}"

        wrapped = wrap_tool("send_mail", send)
        self.assertEqual(wrapped(), "sent-1")
        self.assertEqual(wrapped(), "sent-2")
        self.assertEqual(calls["n"], 2, "side-effecting tool executed every call")


class IdempotentOptInTest(ToolTestBase):
    def test_inherits_defaults_outbound_and_retries_conn(self) -> None:
        # The composed Level 0 policy has no [defaults.tool] (frozen pack), so
        # tool:lookup resolves to [defaults.outbound]: retry on conn applies to
        # the declared-idempotent tool. Stub waits are virtual — never slept.
        backend, _ = self.install(apply_pack_defaults({}))
        calls = {"n": 0}

        def lookup() -> str:
            calls["n"] += 1
            if calls["n"] == 1:
                raise ConnectionError("reset")
            return "found"

        wrapped = wrap_tool("lookup", lookup, idempotent=True)
        self.assertEqual(wrapped(), "found")
        self.assertEqual(calls["n"], 2, "retried per defaults.outbound")
        stats = backend.report()["targets"]["tool:lookup"]
        self.assertEqual(stats["retries"], 1)
        self.assertEqual(stats["successes"], 1)

    def test_success_returns_live_object_identity(self) -> None:
        self.install(apply_pack_defaults({}))
        sentinel = {"nested": (1, 2)}  # tuple survives only via live delivery
        wrapped = wrap_tool("read_cfg", lambda: sentinel, idempotent=True)
        self.assertIs(wrapped(), sentinel)

    def test_cache_hit_with_explicit_target_ttl(self) -> None:
        self.install({"target": {"tool:lookup": {"cache": {"ttl": "60s"}}}})
        calls = {"n": 0}

        def lookup(key: str) -> dict[str, Any]:
            calls["n"] += 1
            return {"key": key, "n": calls["n"]}

        wrapped = wrap_tool("lookup", lookup, idempotent=True)
        first = wrapped("k")
        second = wrapped("k")
        self.assertEqual(calls["n"], 1, "identical args replay from cache")
        self.assertEqual(second, {"key": "k", "n": 1})
        self.assertEqual(first, second)
        self.assertEqual(wrapped("other")["n"], 2, "different args miss the cache")

    def test_breaker_fast_fail_synthesizes_keel_error_e012(self) -> None:
        # A fast-fail has no side-band original: the wrapper raises a KeelError
        # carrying the core's own code (KEEL-E012).
        self.install(
            {
                "target": {
                    "tool:lookup": {
                        "retry": {"attempts": 1, "on": ["conn"], "schedule": "fixed(1ms)"},
                        "breaker": {"failures": 2, "cooldown": "30s"},
                    }
                }
            }
        )
        calls = {"n": 0}

        def lookup() -> None:
            calls["n"] += 1
            raise ConnectionError("refused")

        wrapped = wrap_tool("lookup", lookup, idempotent=True)
        for _ in range(2):
            with self.assertRaises(ConnectionError):
                wrapped()
        with self.assertRaises(KeelError) as ctx:
            wrapped()
        self.assertEqual(ctx.exception.code, "KEEL-E012")
        self.assertEqual(calls["n"], 2, "open breaker fails fast — the tool is not touched")


class AsyncToolTest(ToolTestBase):
    def test_async_tool_retried_on_stub_backend(self) -> None:
        backend, _ = self.install(apply_pack_defaults({}))
        calls = {"n": 0}

        async def fetch_page() -> str:
            calls["n"] += 1
            if calls["n"] == 1:
                raise ConnectionError("reset")
            return "page"

        wrapped = wrap_tool("fetch_page", fetch_page, idempotent=True)
        self.assertEqual(asyncio.run(wrapped()), "page")
        self.assertEqual(calls["n"], 2)
        self.assertEqual(backend.report()["targets"]["tool:fetch_page"]["retries"], 1)

    def test_async_non_idempotent_default_e014(self) -> None:
        self.install({"target": {"tool:post_msg": {"retry": _RETRY_CONN}}})
        original = ConnectionError("reset")
        calls = {"n": 0}

        async def post_msg() -> None:
            calls["n"] += 1
            raise original

        wrapped = wrap_tool("post_msg", post_msg)
        with self.assertRaises(ConnectionError) as ctx:
            asyncio.run(wrapped())
        self.assertIs(ctx.exception, original)
        self.assertEqual(calls["n"], 1)
        self.assertEqual(ctx.exception.keel_outcome["error"]["code"], "KEEL-E014")

    def test_async_passthrough_when_disabled(self) -> None:
        async def echo(x: int) -> int:
            return x * 2

        wrapped = wrap_tool("echo", echo)
        self.assertEqual(asyncio.run(wrapped(21)), 42)  # no backend → transparent


class DisabledAndMarkersTest(ToolTestBase):
    def test_sync_passthrough_when_disabled(self) -> None:
        wrapped = wrap_tool("echo", lambda x: x + 1)
        self.assertEqual(wrapped(1), 2)  # no backend → transparent

    def test_wrapper_markers_and_metadata(self) -> None:
        def my_tool() -> None:
            """docs survive."""

        wrapped = wrap_tool("my_tool", my_tool)
        self.assertTrue(getattr(wrapped, WRAPPED_ATTR))
        self.assertEqual(getattr(wrapped, TARGET_ATTR), "tool:my_tool")
        self.assertEqual(wrapped.__name__, "my_tool")  # functools.wraps: frameworks
        self.assertEqual(wrapped.__doc__, "docs survive.")  # introspect tool functions

    def test_discovery_rows_recorded_under_tool_target(self) -> None:
        _, discovery = self.install({"target": {"tool:ok": {}}})
        wrap_tool("ok", lambda: 1, idempotent=True)()

        def boom() -> None:
            raise RuntimeError("x")

        with self.assertRaises(RuntimeError):
            wrap_tool("boom", boom)()
        rows = self.read_rows(discovery)
        self.assertEqual(rows["tool:ok"]["successes"], 1)
        self.assertEqual(rows["tool:boom"]["failures"], 1)
        self.assertEqual(rows["tool:boom"]["last_error_class"], "other")


if __name__ == "__main__":
    unittest.main()
