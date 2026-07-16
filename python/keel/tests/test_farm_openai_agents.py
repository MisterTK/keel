"""Farm contract test: keel.packs.openai_agents_pack against the REAL
openai-agents SDK.

Runs ONLY under KEEL_ADAPTER_FARM=1 (see test_farm_adk.py's module docs for
the full convention). The offline fast path is
tests/test_packs_openai_agents.py against a structural fake. This module
certifies, on the real package (openai-agents 0.18.2):

* ``agents.FunctionTool`` is a dataclass with a ``__post_init__`` hook
  (openai_agents_pack.py:137-143's shape-check before patching);
* the ``openai-agents`` distribution being present makes ``detect()`` report
  pinned (openai_agents_pack.py:61-62's ``require_dist=True``: a bare
  ``agents`` module is not proof of the SDK);
* after ``install()``, constructing a real ``FunctionTool`` (via the
  ``@agents.function_tool`` decorator, openai_agents_pack.py:6-8's documented
  seam) wraps ``on_invoke_tool`` per instance;
* awaiting ``on_invoke_tool(ctx, "{}")`` on a real, wrapped tool routes
  through Keel (a discovery row appears under ``tool:<name>``);
* ``uninstall()`` restores the instance's original ``on_invoke_tool``
  (openai_agents_pack.py:158-174).

Adjustment made against the real 0.18.2 API: ``on_invoke_tool`` reads
attributes off its ``ctx`` argument (``ctx.tool_name``, ``ctx.run_config``) —
passing ``None`` (as a quick smoke test might) raises ``AttributeError``
inside the SDK's own error-formatting path before Keel's wrapper is even
reached. The tests below construct a minimal real
``agents.tool_context.ToolContext(context=None, tool_name=..., tool_call_id=...,
tool_arguments=...)`` instead — the seam signature and wrapping mechanics
documented in the pack are otherwise unchanged.
"""

from __future__ import annotations

import asyncio
import dataclasses
import os
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

FARM = os.environ.get("KEEL_ADAPTER_FARM") == "1"
SKIP = "KEEL_ADAPTER_FARM=1 not set (offline fast path — see test_packs_openai_agents.py)"

if FARM:  # real imports only in farm mode — never at fast-path collection time
    import agents
    from agents.tool_context import ToolContext

from keel import _runtime
from keel._backend import load_backend
from keel._discovery import Discovery
from keel.packs import openai_agents_pack as pack


def _ctx(tool_name: str, arguments: str = "{}") -> "ToolContext":
    return ToolContext(context=None, tool_name=tool_name, tool_call_id="call_1", tool_arguments=arguments)


@unittest.skipUnless(FARM, SKIP)
class OpenAiAgentsFarmContractTest(unittest.TestCase):
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

    def test_function_tool_is_a_dataclass_with_post_init(self) -> None:
        self.assertTrue(dataclasses.is_dataclass(agents.FunctionTool))
        self.assertTrue(hasattr(agents.FunctionTool, "__post_init__"))

    def test_detect_reports_pinned_on_the_installed_version(self) -> None:
        det = pack.detect()
        self.assertTrue(det.matched)
        self.assertEqual(det.confidence, "pinned", f"real version {det.version} fell out of range")

    def test_install_uninstall_round_trips_on_the_real_sdk(self) -> None:
        pack.uninstall()
        pristine = agents.FunctionTool.__post_init__
        pack.install()
        self.assertIsNot(agents.FunctionTool.__post_init__, pristine)
        pack.uninstall()
        self.assertIs(agents.FunctionTool.__post_init__, pristine)
        pack.install()  # leave installed for tearDown symmetry

    def test_real_function_tool_wraps_and_dispatches_through_keel(self) -> None:
        @agents.function_tool
        def get_weather(city: str) -> str:
            """Get the weather for a city."""
            return f"sunny in {city}"

        self.assertTrue(getattr(get_weather.on_invoke_tool, "__keel_wrapped__", False))
        result = asyncio.run(get_weather.on_invoke_tool(_ctx("get_weather"), '{"city": "nyc"}'))
        self.assertEqual(result, "sunny in nyc")
        stats = _runtime.get_backend().report()["targets"]["tool:get_weather"]
        self.assertEqual(stats["successes"], 1)

    def test_invalid_tool_name_passes_through_unwrapped(self) -> None:
        @agents.function_tool(name_override="Delegate work to coworker")
        def delegate(msg: str) -> str:
            return f"delegated: {msg}"

        self.assertFalse(getattr(delegate.on_invoke_tool, "__keel_wrapped__", False))
        result = asyncio.run(delegate.on_invoke_tool(_ctx(delegate.name), '{"msg": "hi"}'))
        self.assertEqual(result, "delegated: hi")
        self.assertIn("Delegate work to coworker", pack.SKIPPED)

    def test_uninstall_restores_the_instance_callable(self) -> None:
        @agents.function_tool
        def get_weather(city: str) -> str:
            return f"sunny in {city}"

        wrapped = get_weather.on_invoke_tool
        pack.uninstall()
        self.assertIsNot(get_weather.on_invoke_tool, wrapped)
        pack.install()  # leave installed for tearDown symmetry


if __name__ == "__main__":
    unittest.main()
