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
import sqlite3
import unittest
from importlib import metadata
from pathlib import Path
from tempfile import TemporaryDirectory
from typing import Any
from unittest import mock

from fake_adk import FakeAdkModules, FakeApp, FakeBasePlugin, FakeInMemoryRunner, FakeRunner, FakeTool

from keel import _runtime
from keel._backend import load_backend
from keel._discovery import Discovery
from keel._errors import KeelError
from keel.adapters import available_packs, install_adapters, uninstall_adapters
from keel.packs import adk_pack


class AdkTestBase(unittest.TestCase):
    def setUp(self) -> None:
        adk_pack._noted_skips.clear()
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
    """`keel.adapters.install_adapters(extra=(adk_pack,))` — the real
    `bootstrap.install_keel` call shape — arms adk_pack through the identical
    lazy mechanism as httpx/requests, without perturbing `available_packs()`.
    """

    def tearDown(self) -> None:
        with FakeAdkModules():
            uninstall_adapters()

    def test_extra_pack_armed_without_touching_available_packs(self) -> None:
        with FakeAdkModules():
            from google.adk.runners import Runner

            pristine = Runner.__init__
            present = install_adapters(extra=(adk_pack,))
            self.assertIn("google-adk", {d.name for d in present})
            self.assertIsNot(Runner.__init__, pristine, "retroactively patched: already imported")
            # available_packs() is httpx/requests-only regardless of `extra`.
            self.assertEqual({d.name for d in available_packs()}, {"httpx", "requests"})
            uninstall_adapters()
            self.assertIs(Runner.__init__, pristine)


class ToolWrappingTest(AdkTestBase):
    def plugin(self) -> Any:
        with FakeAdkModules():
            return adk_pack._plugin()  # lazily built against the fake BasePlugin

    def test_valid_tool_success_short_circuits_with_dict_result(self) -> None:
        self.install_runtime({"target": {"tool:get_weather": {}}})
        tool = FakeTool("get_weather", lambda city: {"forecast": f"sunny in {city}"})
        plugin = self.plugin()
        result = asyncio.run(
            plugin.before_tool_callback(tool=tool, tool_args={"city": "nyc"}, tool_context=object())
        )
        self.assertEqual(result, {"forecast": "sunny in nyc"})
        self.assertEqual(tool.calls, 1)

    def test_scalar_return_normalized_like_adks_own_build_response_event(self) -> None:
        self.install_runtime({"target": {"tool:count": {}}})
        tool = FakeTool("count", lambda: 42)
        result = asyncio.run(
            self.plugin().before_tool_callback(tool=tool, tool_args={}, tool_context=object())
        )
        self.assertEqual(result, {"result": 42})

    def test_none_return_still_short_circuits_unambiguously(self) -> None:
        # A legitimate `None` tool result must still read as "handled" to
        # ADK's `is None` short-circuit check, not "no override".
        self.install_runtime({"target": {"tool:noop": {}}})
        tool = FakeTool("noop", lambda: None)
        result = asyncio.run(
            self.plugin().before_tool_callback(tool=tool, tool_args={}, tool_context=object())
        )
        self.assertIsNotNone(result)
        self.assertEqual(result, {"result": None})

    def test_non_idempotent_default_not_retried_e014(self) -> None:
        _, discovery = self.install_runtime(
            {"target": {"tool:charge_card": {"retry": {"attempts": 3, "on": ["conn"], "schedule": "fixed(1ms)"}}}}
        )
        original = ConnectionError("reset")

        def charge() -> None:
            raise original

        tool = FakeTool("charge_card", charge)
        with self.assertRaises(ConnectionError) as ctx:
            asyncio.run(
                self.plugin().before_tool_callback(tool=tool, tool_args={}, tool_context=object())
            )
        self.assertIs(ctx.exception, original)
        self.assertEqual(tool.calls, 1, "a side-effecting ADK tool is never auto-retried")
        self.assertEqual(ctx.exception.keel_outcome["error"]["code"], "KEEL-E014")
        rows = self.read_rows(discovery)
        self.assertEqual(rows["tool:charge_card"]["failures"], 1)

    def test_async_tool_function_supported(self) -> None:
        self.install_runtime({"target": {"tool:fetch": {}}})

        async def fetch() -> dict[str, str]:
            return {"page": "1"}

        tool = FakeTool("fetch", fetch)
        result = asyncio.run(
            self.plugin().before_tool_callback(tool=tool, tool_args={}, tool_context=object())
        )
        self.assertEqual(result, {"page": "1"})

    def test_invalid_tool_name_skipped_unwrapped(self) -> None:
        self.install_runtime({})
        tool = FakeTool("get weather", lambda: {"ok": True})  # space: not a valid tool: name
        result = asyncio.run(
            self.plugin().before_tool_callback(tool=tool, tool_args={}, tool_context=object())
        )
        self.assertIsNone(result, "no override: ADK invokes the tool itself, unwrapped")
        self.assertEqual(tool.calls, 0, "the plugin never called the tool directly")

    def test_missing_or_non_string_name_skipped(self) -> None:
        self.install_runtime({})
        tool = FakeTool("", lambda: 1)
        tool.name = None  # some exotic tool object
        result = asyncio.run(
            self.plugin().before_tool_callback(tool=tool, tool_args={}, tool_context=object())
        )
        self.assertIsNone(result)


if __name__ == "__main__":
    unittest.main()
