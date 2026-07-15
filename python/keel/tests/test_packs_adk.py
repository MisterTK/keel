"""The Google ADK framework pack (dx-spec §4.2).

Runs entirely OFFLINE against the structural fake in ``fixtures/fake_adk.py``
(signatures verified against the real ``google-adk`` 2.4.0 in a throwaway
venv — never a repo dependency, mirrors how ``node/keel/test/ai-sdk.test.mjs``
tests the AI SDK pack against a fake model). Covers: honest detection, the
adapter-pack contract shape, install/uninstall reversibility on
``Runner.__init__``, zero-code-change plugin auto-registration across every
ADK construction shape (``Runner(agent=...)``, ``InMemoryRunner(...)``,
``Runner(app=App(plugins=[...]))``), and the ``before_tool_callback`` seam's
Level 0 semantics (non-idempotent by default, skip-and-note for an unwrappable
tool name, discovery recording).
"""

from __future__ import annotations

import asyncio
import io
import os
import sqlite3
import sys
import unittest
from importlib import metadata
from pathlib import Path
from tempfile import TemporaryDirectory
from typing import Any
from unittest import mock

from fake_adk import (
    FakeAdkModules,
    FakeApp,
    FakeBasePlugin,
    FakeInMemoryRunner,
    FakeRunner,
    FakeTool,
    FakeSlottedTool,
    McpTool,
)

from keel import _runtime
from keel._backend import load_backend
from keel._discovery import Discovery
from keel._errors import KeelError
from keel.adapters import available_packs, install_adapters, uninstall_adapters
from keel.packs import adk_pack


class AdkTestBase(unittest.TestCase):
    def setUp(self) -> None:
        adk_pack._noted_skips.clear()
        adk_pack._noted_fallbacks.clear()
        adk_pack._rebound.clear()
        self._tmp = TemporaryDirectory()
        self.cwd = Path(self._tmp.name)
        self._discoveries: list[Discovery] = []

    def tearDown(self) -> None:
        _runtime.clear_runtime()
        for d in self._discoveries:
            d.close()
        self._tmp.cleanup()

    def install_runtime(self, policy: dict[str, Any]) -> tuple[Any, Discovery]:
        backend = load_backend("stub")
        backend.configure(policy)
        discovery = Discovery(self.cwd)
        self._discoveries.append(discovery)
        _runtime.set_runtime(backend, discovery)
        return backend, discovery

    def read_rows(self, discovery: Discovery) -> dict[str, sqlite3.Row]:
        discovery.close()
        conn = sqlite3.connect(discovery.db_path)
        conn.row_factory = sqlite3.Row
        try:
            return {r["target"]: r for r in conn.execute("SELECT * FROM discovery")}
        finally:
            conn.close()


class DetectTest(unittest.TestCase):
    def test_absent_is_honestly_unmatched(self) -> None:
        # google.adk is genuinely not installed in this test environment.
        det = adk_pack.detect()
        self.assertFalse(det.matched)

    def test_present_but_no_distribution_metadata_is_best_effort(self) -> None:
        with FakeAdkModules():
            det = adk_pack.detect()
        self.assertTrue(det.matched)
        self.assertEqual(det.name, "google-adk")
        self.assertEqual(det.confidence, "best_effort")

    def test_pinned_version_reports_pinned(self) -> None:
        with FakeAdkModules(), mock.patch.object(metadata, "version", return_value="2.4.0"):
            det = adk_pack.detect()
        self.assertEqual(det.version, "2.4.0")
        self.assertEqual(det.confidence, "pinned")

    def test_unpinned_major_is_best_effort(self) -> None:
        with FakeAdkModules(), mock.patch.object(metadata, "version", return_value="0.9.0"):
            det = adk_pack.detect()
        self.assertEqual(det.confidence, "best_effort")


class ContractShapeTest(unittest.TestCase):
    def test_seams_documents_runner_init(self) -> None:
        seams = adk_pack.seams()
        self.assertEqual(len(seams), 1)
        self.assertIn("Runner.__init__", seams[0].patch_point)

    def test_targets_declares_tool_and_llm(self) -> None:
        decls = {d.kind: d for d in adk_pack.targets()}
        self.assertEqual(decls["tool"].pattern, "tool:<name>")
        self.assertIn("non-idempotent by default", decls["tool"].idempotency_rule)
        self.assertEqual(decls["llm"].pattern, "llm:google-genai")
        self.assertIn("httpx_pack", decls["llm"].idempotency_rule)

    def test_defaults_empty(self) -> None:
        # No [defaults.adk] in the frozen pack (tool:/mcp: pattern).
        self.assertEqual(adk_pack.defaults(), {})


class InstallReversibilityTest(unittest.TestCase):
    def tearDown(self) -> None:
        # Belt-and-suspenders: each test below already uninstalls inside its
        # own `with FakeAdkModules()` block, but re-enter here too in case a
        # failing assertion skipped that — never leave `_installed` stuck.
        with FakeAdkModules():
            adk_pack.uninstall()

    def test_install_patches_runner_init_and_uninstall_restores_it(self) -> None:
        with FakeAdkModules():
            from google.adk.runners import Runner  # the fake, via sys.modules

            pristine = Runner.__init__
            adk_pack.install()
            self.assertIsNot(Runner.__init__, pristine, "Runner.__init__ patched")
            self.assertTrue(getattr(Runner.__init__, "__keel_wrapped__", False))
            adk_pack.uninstall()
            self.assertIs(Runner.__init__, pristine, "Runner.__init__ restored")

    def test_install_is_idempotent_and_noop_when_absent(self) -> None:
        adk_pack.install()  # google.adk genuinely absent here: a no-op
        adk_pack.uninstall()  # never armed: also a no-op, must not raise
        with FakeAdkModules():
            from google.adk.runners import Runner

            pristine = Runner.__init__
            adk_pack.install()
            adk_pack.install()  # second call: no double-patch
            self.assertTrue(getattr(Runner.__init__, "__keel_wrapped__", False))
            wrapped_once = Runner.__init__
            adk_pack.install()
            self.assertIs(Runner.__init__, wrapped_once)
            adk_pack.uninstall()
            self.assertIs(Runner.__init__, pristine)


class PluginAutoRegistrationTest(unittest.TestCase):
    def tearDown(self) -> None:
        # FakeRunner/FakeInMemoryRunner are shared class objects across every
        # `with FakeAdkModules()` block (fixture module-level singletons), so
        # a patch applied inside one block persists on that same class object
        # after the block exits — re-enter the fake context here so
        # `uninstall()` (a no-op if `install()` was never called) restores it
        # correctly rather than leaking into the next test.
        with FakeAdkModules():
            adk_pack.uninstall()

    def test_runner_agent_construction_gets_the_plugin(self) -> None:
        with FakeAdkModules():
            adk_pack.install()
            from google.adk.runners import Runner

            runner = Runner(app_name="app", agent=None)
            plugin = runner.plugin_manager.get_plugin(adk_pack.PLUGIN_NAME)
            self.assertIsNotNone(plugin)
            self.assertEqual(plugin.name, "keel")

    def test_in_memory_runner_construction_gets_the_plugin(self) -> None:
        # InMemoryRunner.__init__ forwards to Runner.__init__ via super() —
        # patching only Runner.__init__ must cover this shape too.
        with FakeAdkModules():
            adk_pack.install()
            from google.adk.runners import InMemoryRunner

            runner = InMemoryRunner(agent=None, app_name="app")
            self.assertIsNotNone(runner.plugin_manager.get_plugin(adk_pack.PLUGIN_NAME))

    def test_app_plugins_construction_shape_gets_the_plugin(self) -> None:
        # The modern, recommended Runner(app=App(plugins=[...])) shape — the
        # deprecated plugins= kwarg is not used here at all.
        with FakeAdkModules():
            adk_pack.install()
            from google.adk.runners import Runner

            app = FakeApp(name="app", plugins=[])
            runner = Runner(app=app)
            self.assertIsNotNone(runner.plugin_manager.get_plugin(adk_pack.PLUGIN_NAME))

    def test_users_own_keel_named_plugin_is_not_clobbered(self) -> None:
        with FakeAdkModules():
            adk_pack.install()
            from google.adk.runners import Runner

            mine = FakeBasePlugin("keel")
            runner = Runner(app_name="app", agent=None, plugins=[mine])
            self.assertIs(runner.plugin_manager.get_plugin("keel"), mine)

    def test_two_runners_each_get_the_shared_plugin_instance(self) -> None:
        with FakeAdkModules():
            adk_pack.install()
            from google.adk.runners import Runner

            r1 = Runner(app_name="a", agent=None)
            r2 = Runner(app_name="b", agent=None)
            p1 = r1.plugin_manager.get_plugin("keel")
            p2 = r2.plugin_manager.get_plugin("keel")
            self.assertIs(p1, p2, "one shared KeelPlugin singleton, registered on every runner")

    def test_attach_plugin_defensive_when_plugin_manager_missing(self) -> None:
        # A hypothetical/unsupported ADK shape whose Runner never sets
        # `self.plugin_manager` at all: `_attach_plugin` must do nothing
        # unsafe, not raise (Level 0: "if a call site cannot be wrapped
        # safely, do nothing").
        class NoPluginManager:
            pass

        adk_pack._attach_plugin(NoPluginManager())  # must not raise

    def test_attach_plugin_defensive_when_plugin_manager_is_the_wrong_shape(self) -> None:
        class HalfBaked:
            plugin_manager = object()  # present, but not a real PluginManager

        adk_pack._attach_plugin(HalfBaked())  # must not raise


class InstallAdaptersIntegrationTest(unittest.TestCase):
    """`keel.adapters.install_adapters()` — the real `bootstrap.install_keel`
    call shape — arms adk_pack through the identical lazy mechanism as
    httpx/requests, registered like every other framework pack (`keel.
    adapters._framework_packs`), so it also shows up in `available_packs()`.
    """

    def tearDown(self) -> None:
        with FakeAdkModules():
            uninstall_adapters()

    def test_adk_pack_armed_and_reported_like_any_framework_pack(self) -> None:
        with FakeAdkModules():
            from google.adk.runners import Runner

            pristine = Runner.__init__
            present = install_adapters()
            self.assertIn("google-adk", {d.name for d in present})
            self.assertIsNot(Runner.__init__, pristine, "retroactively patched: already imported")
            self.assertIn("google-adk", {d.name for d in available_packs() if d.matched})
            uninstall_adapters()
            self.assertIs(Runner.__init__, pristine)


class ToolWrappingTest(AdkTestBase):
    """The redesigned seam: before_tool_callback REBINDS tool.run_async on
    first sight and returns None, so ADK's own sequence (agent-level
    before-callbacks -> real call -> on_tool_error path) proceeds unchanged
    with Keel wrapped directly around the real call."""

    def plugin(self) -> Any:
        with FakeAdkModules():
            return adk_pack._plugin()  # lazily built against the fake BasePlugin

    def see(self, plugin: Any, tool: Any, tool_args: dict[str, Any] | None = None) -> Any:
        """One before_tool_callback invocation (ADK's step 1)."""
        return asyncio.run(
            plugin.before_tool_callback(tool=tool, tool_args=tool_args or {}, tool_context=object())
        )

    def test_first_sight_rebinds_and_returns_none(self) -> None:
        # Returning None is the contract that preserves agent-level
        # before_tool_callbacks: ADK only skips them on a non-None return.
        self.install_runtime({"target": {"tool:get_weather": {}}})
        tool = FakeTool("get_weather", lambda city: {"forecast": f"sunny in {city}"})
        self.assertIsNone(self.see(self.plugin(), tool))
        self.assertTrue(getattr(tool.run_async, adk_pack._REBOUND_ATTR, False))
        self.assertEqual(tool.calls, 0, "the callback never executes the tool itself")

    def test_rebound_run_async_returns_raw_result(self) -> None:
        # Keel now sits BELOW ADK's __build_response_event, so results pass
        # through raw — dicts and scalars alike; ADK normalizes above us.
        self.install_runtime({"target": {"tool:get_weather": {}}})
        plugin = self.plugin()
        tool = FakeTool("get_weather", lambda city: {"forecast": f"sunny in {city}"})
        self.see(plugin, tool)
        result = asyncio.run(tool.run_async(args={"city": "nyc"}, tool_context=object()))
        self.assertEqual(result, {"forecast": "sunny in nyc"})
        self.assertEqual(tool.calls, 1)
        scalar = FakeTool("count", lambda: 42)
        self.see(plugin, scalar)
        self.assertEqual(asyncio.run(scalar.run_async(args={}, tool_context=object())), 42)

    def test_second_sight_does_not_double_wrap(self) -> None:
        self.install_runtime({"target": {"tool:get_weather": {}}})
        plugin = self.plugin()
        tool = FakeTool("get_weather", lambda city: city)
        self.see(plugin, tool)
        wrapped_once = tool.run_async
        self.assertIsNone(self.see(plugin, tool))
        self.assertIs(tool.run_async, wrapped_once, "second sight is a no-op")

    def test_non_idempotent_default_not_retried_e014_and_error_raises_at_the_real_call(self) -> None:
        _, discovery = self.install_runtime(
            {"target": {"tool:charge_card": {"retry": {"attempts": 3, "on": ["conn"], "schedule": "fixed(1ms)"}}}}
        )
        original = ConnectionError("reset")

        def charge() -> None:
            raise original

        tool = FakeTool("charge_card", charge)
        plugin = self.plugin()
        # Step 1 (before_tool_callback) must NOT raise — the failure has to
        # surface from the real call (ADK's step 3), inside ADK's
        # try/except, so user on_tool_error handlers fire again.
        self.assertIsNone(self.see(plugin, tool))
        with self.assertRaises(ConnectionError) as ctx:
            asyncio.run(tool.run_async(args={}, tool_context=object()))
        self.assertIs(ctx.exception, original, "original exception, not RuntimeError-wrapped")
        self.assertEqual(tool.calls, 1, "a side-effecting ADK tool is never auto-retried")
        self.assertEqual(ctx.exception.keel_outcome["error"]["code"], "KEEL-E014")
        rows = self.read_rows(discovery)
        self.assertEqual(rows["tool:charge_card"]["failures"], 1)

    def test_async_tool_function_supported(self) -> None:
        self.install_runtime({"target": {"tool:fetch": {}}})

        async def fetch() -> dict[str, str]:
            return {"page": "1"}

        tool = FakeTool("fetch", fetch)
        plugin = self.plugin()
        self.see(plugin, tool)
        self.assertEqual(asyncio.run(tool.run_async(args={}, tool_context=object())), {"page": "1"})

    def test_invalid_tool_name_skipped_unwrapped(self) -> None:
        self.install_runtime({})
        tool = FakeTool("get weather", lambda: {"ok": True})  # space: not a valid tool: name
        self.assertIsNone(self.see(self.plugin(), tool))
        self.assertFalse(getattr(tool.run_async, adk_pack._REBOUND_ATTR, False))
        self.assertEqual(tool.calls, 0)

    def test_missing_or_non_string_name_skipped(self) -> None:
        self.install_runtime({})
        tool = FakeTool("", lambda: 1)
        tool.name = None  # some exotic tool object
        self.assertIsNone(self.see(self.plugin(), tool))
        self.assertFalse(getattr(tool.run_async, adk_pack._REBOUND_ATTR, False))


class RebindLifecycleTest(AdkTestBase):
    def test_uninstall_restores_rebound_instances(self) -> None:
        self.install_runtime({"target": {"tool:get_weather": {}}})
        with FakeAdkModules():
            adk_pack.install()
            tool = FakeTool("get_weather", lambda city: city)
            asyncio.run(
                adk_pack._plugin().before_tool_callback(
                    tool=tool, tool_args={}, tool_context=object()
                )
            )
            self.assertIn("run_async", tool.__dict__, "rebind shadows the class method")
            adk_pack.uninstall()
            self.assertNotIn("run_async", tool.__dict__, "shadow removed: class method restored")
            self.assertEqual(asyncio.run(tool.run_async(args={"city": "x"}, tool_context=object())), "x")


class SetattrFallbackTest(AdkTestBase):
    """A tool instance that rejects rebinding still gets full Keel coverage
    via the old loop-in-callback path — coverage never silently drops."""

    def plugin(self) -> Any:
        with FakeAdkModules():
            return adk_pack._plugin()

    def test_slotted_tool_falls_back_to_plugin_loop_with_normalized_result(self) -> None:
        self.install_runtime({"target": {"tool:count": {}}})
        tool = FakeSlottedTool("count", lambda: 42)
        with mock.patch.object(sys, "stderr", new_callable=io.StringIO) as err:
            result = asyncio.run(
                self.plugin().before_tool_callback(tool=tool, tool_args={}, tool_context=object())
            )
        # Fallback = the pre-redesign contract: executed in the callback,
        # normalized like ADK's own __build_response_event.
        self.assertEqual(result, {"result": 42})
        self.assertEqual(tool.calls, 1)
        self.assertIn("rejects attribute rebinding", err.getvalue())

    def test_fallback_failure_accounting_and_e014(self) -> None:
        _, discovery = self.install_runtime(
            {"target": {"tool:charge": {"retry": {"attempts": 3, "on": ["conn"], "schedule": "fixed(1ms)"}}}}
        )
        original = ConnectionError("reset")

        def charge() -> None:
            raise original

        tool = FakeSlottedTool("charge", charge)
        with self.assertRaises(ConnectionError) as ctx:
            asyncio.run(
                self.plugin().before_tool_callback(tool=tool, tool_args={}, tool_context=object())
            )
        self.assertIs(ctx.exception, original)
        self.assertEqual(tool.calls, 1)
        self.assertEqual(ctx.exception.keel_outcome["error"]["code"], "KEEL-E014")
        self.assertEqual(self.read_rows(discovery)["tool:charge"]["failures"], 1)

    def test_fallback_note_emitted_once_and_quietable(self) -> None:
        self.install_runtime({"target": {"tool:count": {}}})
        plugin = self.plugin()
        tool = FakeSlottedTool("count", lambda: 1)
        with mock.patch.object(sys, "stderr", new_callable=io.StringIO) as err:
            asyncio.run(plugin.before_tool_callback(tool=tool, tool_args={}, tool_context=object()))
            asyncio.run(plugin.before_tool_callback(tool=tool, tool_args={}, tool_context=object()))
        self.assertEqual(err.getvalue().count("rejects attribute rebinding"), 1, "noted once, not per-call")
        adk_pack._noted_fallbacks.clear()
        with mock.patch.dict(os.environ, {"KEEL_QUIET": "1"}), mock.patch.object(
            sys, "stderr", new_callable=io.StringIO
        ) as err:
            asyncio.run(plugin.before_tool_callback(tool=tool, tool_args={}, tool_context=object()))
        self.assertEqual(err.getvalue(), "")


class McpErrorDictTest(AdkTestBase):
    """ADK graceful error handling returns {"error": ...} dicts from McpTool
    — a *successful* call from a naive wrapper's perspective. Keel must
    count it as a failure (breaker/discovery) while returning it unchanged."""

    def plugin(self) -> Any:
        with FakeAdkModules():
            return adk_pack._plugin()

    def rebound(self, tool: Any) -> Any:
        plugin = self.plugin()
        asyncio.run(plugin.before_tool_callback(tool=tool, tool_args={}, tool_context=object()))
        return tool

    def test_error_dict_counts_as_failure_but_returns_unchanged(self) -> None:
        _, discovery = self.install_runtime({"target": {"tool:mcp_search": {}}})
        tool = self.rebound(McpTool("mcp_search", lambda: {"error": "connection closed"}))
        result = asyncio.run(tool.run_async(args={}, tool_context=object()))
        self.assertEqual(result, {"error": "connection closed"}, "agent-visible value unchanged")
        self.assertEqual(tool.calls, 1, "never re-invoked: tools stay non-idempotent")
        rows = self.read_rows(discovery)
        self.assertEqual(rows["tool:mcp_search"]["failures"], 1, "breaker/discovery sees the failure")

    def test_successful_mcp_result_recorded_as_success(self) -> None:
        _, discovery = self.install_runtime({"target": {"tool:mcp_search": {}}})
        tool = self.rebound(McpTool("mcp_search", lambda: {"content": ["hit"]}))
        result = asyncio.run(tool.run_async(args={}, tool_context=object()))
        self.assertEqual(result, {"content": ["hit"]})
        rows = self.read_rows(discovery)
        self.assertEqual(rows["tool:mcp_search"]["failures"], 0)

    def test_non_mcp_tool_error_shaped_dict_is_not_reclassified(self) -> None:
        # A plain FunctionTool legitimately returning {"error": ...} is a
        # RESULT, not a failure — classification applies to McpTool only.
        _, discovery = self.install_runtime({"target": {"tool:validator": {}}})
        tool = self.rebound(FakeTool("validator", lambda: {"error": "field x is required"}))
        result = asyncio.run(tool.run_async(args={}, tool_context=object()))
        self.assertEqual(result, {"error": "field x is required"})
        rows = self.read_rows(discovery)
        self.assertEqual(rows["tool:validator"]["failures"], 0)

    def test_extra_keys_or_non_string_error_is_not_an_error_dict(self) -> None:
        self.assertFalse(adk_pack._is_mcp_error_dict({"error": "x", "detail": "y"}))
        self.assertFalse(adk_pack._is_mcp_error_dict({"error": 500}))
        self.assertFalse(adk_pack._is_mcp_error_dict(["error"]))
        self.assertTrue(adk_pack._is_mcp_error_dict({"error": "x"}))


if __name__ == "__main__":
    unittest.main()
