"""Farm contract test: keel.packs.pydantic_ai_pack against the REAL
pydantic-ai.

Runs ONLY under KEEL_ADAPTER_FARM=1 (see test_farm_adk.py's module docs for
the full convention). The offline fast path is tests/test_packs_pydantic_ai.py
against a structural fake. This module certifies, on the real package
(pydantic-ai 2.9.0, pydantic-ai-slim):

* ``pydantic_ai.toolsets`` exposes ``FunctionToolset`` (pydantic_ai_pack.py:34
  imports it inside ``install()``);
* ``FunctionToolset.call_tool`` exists and is a coroutine function
  (pydantic_ai_pack.py:3-9's documented seam: ``AbstractToolset.call_tool(name,
  tool_args, ctx, tool) -> Any``, "below the model-request loop");
* after ``install()``, a real ``FunctionToolset`` with one registered function
  dispatches through Keel end-to-end (a discovery row appears under
  ``tool:<name>``) — driven through a real ``Agent`` run with
  ``pydantic_ai.models.test.TestModel`` so the call reaches ``call_tool`` via
  the framework's own tool-invocation path, not a hand-built ``ctx``/``tool``;
* an invalid ``tool:`` name (pydantic_ai_pack.py:144's
  ``is_valid_tool_name`` check) passes through unwrapped and is recorded in
  ``pack.SKIPPED`` (pydantic_ai_pack.py:88-99's documented "skip and note"
  contract), rather than raising mid-run.

No adjustment to the pack's calls was needed against the real 2.9.0 API — the
seam signature matches the module's own documentation exactly. One adjustment
to the TEST driving code: ``toolset.add_function`` requires an explicit
``takes_ctx=False`` for a plain function with no ``RunContext`` parameter —
pydantic-ai's schema generator otherwise raises on the auto-detection path
(observed against the real package; unrelated to the pack's own logic, which
never touches ``takes_ctx``).
"""

from __future__ import annotations

import asyncio
import inspect
import os
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

FARM = os.environ.get("KEEL_ADAPTER_FARM") == "1"
SKIP = "KEEL_ADAPTER_FARM=1 not set (offline fast path — see test_packs_pydantic_ai.py)"

if FARM:  # real imports only in farm mode — never at fast-path collection time
    from pydantic_ai import Agent
    from pydantic_ai.models.test import TestModel
    from pydantic_ai.toolsets import FunctionToolset

from keel import _runtime
from keel._backend import load_backend
from keel._discovery import Discovery
from keel.packs import pydantic_ai_pack as pack


def _get_weather(city: str) -> str:
    return f"sunny in {city}"


@unittest.skipUnless(FARM, SKIP)
class PydanticAiFarmContractTest(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = TemporaryDirectory()
        backend = load_backend("stub")
        backend.configure({"target": {"tool:get_weather": {}}})
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

    def test_call_tool_seam_shape(self) -> None:
        self.assertTrue(hasattr(FunctionToolset, "call_tool"))
        self.assertTrue(inspect.iscoroutinefunction(FunctionToolset.call_tool))

    def test_install_uninstall_round_trips_on_the_real_toolset(self) -> None:
        pack.uninstall()
        pristine = FunctionToolset.call_tool
        pack.install()
        self.assertIsNot(FunctionToolset.call_tool, pristine)
        self.assertTrue(getattr(FunctionToolset.call_tool, "__keel_wrapped__", False))
        pack.uninstall()
        self.assertIs(FunctionToolset.call_tool, pristine)
        pack.install()  # leave installed for tearDown symmetry

    def test_real_function_toolset_dispatches_through_keel(self) -> None:
        # Driven through a real Agent + TestModel run, so the call reaches
        # FunctionToolset.call_tool via pydantic-ai's own tool-invocation
        # path (below the model-request loop, per the pack's module docs).
        toolset = FunctionToolset()
        toolset.add_function(_get_weather, takes_ctx=False, name="get_weather")
        agent = Agent(TestModel(call_tools=["get_weather"]), toolsets=[toolset])
        result = asyncio.run(agent.run("what is the weather?"))
        self.assertIn("sunny in", result.output)
        stats = _runtime.get_backend().report()["targets"]["tool:get_weather"]
        self.assertEqual(stats["successes"], 1)

    def test_invalid_tool_name_passes_through_unwrapped(self) -> None:
        toolset = FunctionToolset()
        toolset.add_function(_get_weather, takes_ctx=False, name="get weather")  # space: invalid
        agent = Agent(TestModel(call_tools=["get weather"]), toolsets=[toolset])
        result = asyncio.run(agent.run("what is the weather?"))
        self.assertIn("sunny in", result.output)
        self.assertIn("get weather", pack.SKIPPED)
        self.assertNotIn("tool:get weather", _runtime.get_backend().report()["targets"])


if __name__ == "__main__":
    unittest.main()
