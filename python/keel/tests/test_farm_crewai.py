"""Farm contract test: keel.packs.crewai_pack against the REAL crewai.

Runs ONLY under KEEL_ADAPTER_FARM=1 (see test_farm_adk.py's module docs for
the full convention). The offline fast path is tests/test_packs_crewai.py
against a structural fake. This module certifies, on the real package
(crewai 1.15.2):

* ``crewai.tools.structured_tool.CrewStructuredTool`` has callable ``invoke``
  AND ``ainvoke`` with the ``(self, input, config=None, **kwargs)`` shape the
  pack assumes (crewai_pack.py:3-9, :86-89) — bound via
  ``inspect.signature(...).bind`` against the REAL unbound methods;
* after ``install()``, a real ``CrewStructuredTool`` built via
  ``CrewStructuredTool.from_function`` dispatches through Keel (a discovery
  row appears under ``tool:<name>``), both sync (``invoke``) and async
  (``ainvoke``);
* a raising tool propagates the ORIGINAL exception (crewai_pack.py:14-18's
  documented assumption that ``invoke``/``ainvoke`` do not swallow it
  themselves — unlike the OpenAI Agents SDK's ``on_invoke_tool``).

Adjustment made against the real 1.15.2 API: ``CrewStructuredTool.from_function``
raises ``ValueError: Function <name> must have a docstring if description not
provided`` when neither is given — the tests below pass an explicit
``description=`` for every tool built with a bare (docstring-free) test
function; this is a caller-side requirement of the real constructor, not an
adjustment to the pack's own ``invoke``/``ainvoke`` patch logic, which is
unaffected by how the tool was constructed.
"""

from __future__ import annotations

import asyncio
import inspect
import os
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

FARM = os.environ.get("KEEL_ADAPTER_FARM") == "1"
SKIP = "KEEL_ADAPTER_FARM=1 not set (offline fast path — see test_packs_crewai.py)"

if FARM:  # real imports only in farm mode — never at fast-path collection time
    from crewai.tools.structured_tool import CrewStructuredTool

from keel import _runtime
from keel._backend import load_backend
from keel._discovery import Discovery
from keel.packs import crewai_pack as pack


def _get_weather(city: str) -> str:
    return f"sunny in {city}"


async def _get_weather_async(city: str) -> str:
    return f"sunny async in {city}"


def _boom(city: str) -> str:
    raise ConnectionError("reset")


@unittest.skipUnless(FARM, SKIP)
class CrewAiFarmContractTest(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = TemporaryDirectory()
        backend = load_backend("stub")
        backend.configure(
            {
                "target": {
                    "tool:get_weather": {},
                    "tool:get_weather_async": {},
                    "tool:boom_tool": {"retry": {"attempts": 3, "on": ["conn"], "schedule": "fixed(1ms)"}},
                }
            }
        )
        self.discovery = Discovery(Path(self._tmp.name))
        _runtime.set_runtime(backend, self.discovery)
        pack.install()

    def tearDown(self) -> None:
        pack.uninstall()
        _runtime.clear_runtime()
        self.discovery.close()
        self._tmp.cleanup()

    def test_detect_reports_pinned_on_the_installed_version(self) -> None:
        det = pack.detect()
        self.assertTrue(det.matched)
        self.assertEqual(det.confidence, "pinned", f"real version {det.version} fell out of range")

    def test_invoke_and_ainvoke_signatures_bind_the_assumed_shape(self) -> None:
        sync_sig = inspect.signature(CrewStructuredTool.invoke)
        bound = sync_sig.bind(object(), {"city": "nyc"})
        bound.apply_defaults()
        self.assertIn("input", bound.arguments)
        self.assertIsNone(bound.arguments["config"])

        async_sig = inspect.signature(CrewStructuredTool.ainvoke)
        abound = async_sig.bind(object(), {"city": "nyc"})
        abound.apply_defaults()
        self.assertIn("input", abound.arguments)
        self.assertIsNone(abound.arguments["config"])

    def test_install_uninstall_round_trips_on_the_real_tool_class(self) -> None:
        pack.uninstall()
        pristine_invoke = CrewStructuredTool.invoke
        pristine_ainvoke = CrewStructuredTool.ainvoke
        pack.install()
        self.assertIsNot(CrewStructuredTool.invoke, pristine_invoke)
        self.assertIsNot(CrewStructuredTool.ainvoke, pristine_ainvoke)
        pack.uninstall()
        self.assertIs(CrewStructuredTool.invoke, pristine_invoke)
        self.assertIs(CrewStructuredTool.ainvoke, pristine_ainvoke)
        pack.install()  # leave installed for tearDown symmetry

    def test_real_sync_tool_dispatches_through_keel(self) -> None:
        tool = CrewStructuredTool.from_function(_get_weather, name="get_weather", description="weather lookup")
        result = tool.invoke({"city": "nyc"})
        self.assertEqual(result, "sunny in nyc")
        stats = _runtime.get_backend().report()["targets"]["tool:get_weather"]
        self.assertEqual(stats["successes"], 1)

    def test_real_async_tool_dispatches_through_keel(self) -> None:
        tool = CrewStructuredTool.from_function(
            _get_weather_async, name="get_weather_async", description="async weather lookup"
        )
        result = asyncio.run(tool.ainvoke({"city": "sf"}))
        self.assertEqual(result, "sunny async in sf")
        stats = _runtime.get_backend().report()["targets"]["tool:get_weather_async"]
        self.assertEqual(stats["successes"], 1)

    def test_raising_tool_propagates_original_exception_not_retried(self) -> None:
        tool = CrewStructuredTool.from_function(_boom, name="boom_tool", description="always fails")
        with self.assertRaises(ConnectionError) as ctx:
            tool.invoke({"city": "x"})
        self.assertEqual(str(ctx.exception), "reset")
        self.assertEqual(ctx.exception.keel_outcome["error"]["code"], "KEEL-E014")


if __name__ == "__main__":
    unittest.main()
