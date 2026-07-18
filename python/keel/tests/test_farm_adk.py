"""Farm contract test: keel.packs.adk_pack against the REAL google-adk.

Runs ONLY under KEEL_ADAPTER_FARM=1 (the weekly adapter-farm workflow, which
pip-installs the pinned real library first — see .github/workflows/
adapter-farm.yml). The offline fast path is tests/test_packs_adk.py against
the structural fake. This module certifies the structural assumptions the
fake encodes, on the real package:

* Runner.__init__ is the construction chokepoint (InMemoryRunner forwards);
* plugin_manager exposes register_plugin/get_plugin; BasePlugin subclassing
  + the keyword-only before_tool_callback hook signature;
* install()/uninstall() round-trip on the real Runner;
* rebind-on-first-sight works on a real FunctionTool: callback returns None,
  agent-level callback sequencing is preserved by the REAL PluginManager
  (the WS1 claim that unit tests could only pin at precondition level);
* McpTool class name + graceful-error dict shape assumptions still hold.

Adjustments made against the real google-adk 2.4.0 package (verified in a
throwaway venv, see ws3-task-1-report.md for the full certification log):

* ``InMemoryRunner(agent=None, ...)`` raises ``ValueError: One of app, agent,
  or node must be provided.`` on the real ``Runner._resolve_app`` — the
  structural fake in ``fixtures/fake_adk.py`` permits ``agent=None``, the real
  package does not. A minimal real ``google.adk.agents.BaseAgent(name=...)``
  is constructed instead everywhere the brief's rendering passed ``agent=None``.
* ``FunctionTool(func=lambda city: ...)`` names the tool ``"<lambda>"`` (from
  ``func.__name__``) — not a valid ``tool:<name>`` grammar match
  (``keel.packs.tool.is_valid_tool_name``), so the rebind path never engages
  and ``_REBOUND_ATTR`` never gets set. A plain named function is used instead
  so ``FunctionTool.name`` is a real, wrappable ``get_weather``.
* ``PluginManager.run_before_tool_callback`` and ``McpTool``'s module both
  import cleanly and match the brief's asserted shapes exactly (unbound
  keyword-only ``tool``/``tool_args``/``tool_context``, no further adjustment
  needed) — ``tool_context=None`` is safe: ``PluginManager._run_callbacks``
  only forwards it as a kwarg to each plugin's callback, never touches it
  itself, and Keel's own callback doesn't read it either.
* Importing ``google.adk.tools.mcp_tool.mcp_tool`` requires the ``mcp``
  package (``from mcp.shared.exceptions import McpError``) even though
  ``google-adk``'s own distribution metadata does not declare it as a hard
  dependency — the farm venv installs both ``google-adk==2.4.0`` and the
  pinned ``mcp`` version together (no conflict; see the report).
* WS5 Task 5 (5b): the farm venv deliberately has NO ``anthropic`` package
  installed (it is not a declared dependency of ``google-adk`` — verified via
  ``pip show google-adk``'s ``Requires:`` list, which does not mention it),
  so a ``claude-*`` model name hits ``LLMRegistry.resolve``'s real
  ``ImportError``-swallowing branch and its actionable ``ValueError`` remedy
  ("Claude models require the anthropic package...") for real — exactly the
  resolution-failure shape ``adk_pack._resolve_model_class``'s
  ``note_on_failure=True`` branch exists to skip-and-note rather than crash
  on. ``on_model_error_callback``'s cross-model construction
  (``LLMRegistry.new_llm``) and the real ``PluginManager.
  run_on_model_error_callback`` early-exit-on-non-``None`` dispatch are
  certified below too.
"""

from __future__ import annotations

import asyncio
import inspect
import os
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory
from typing import Any

FARM = os.environ.get("KEEL_ADAPTER_FARM") == "1"
SKIP = "KEEL_ADAPTER_FARM=1 not set (offline fast path — see test_packs_adk.py)"

if FARM:  # real imports only in farm mode — never at fast-path collection time
    from google.adk.agents import BaseAgent
    from google.adk.agents.llm_agent import LlmAgent
    from google.adk.models.base_llm import BaseLlm
    from google.adk.models.llm_request import LlmRequest
    from google.adk.models.llm_response import LlmResponse
    from google.adk.models.registry import LLMRegistry
    from google.adk.plugins.base_plugin import BasePlugin
    from google.adk.runners import InMemoryRunner, Runner
    from google.adk.tools.function_tool import FunctionTool
    from google.genai import types

from keel import _runtime
from keel._backend import load_backend
from keel._discovery import Discovery
from keel.packs import adk_pack


def get_weather(city: str) -> dict[str, str]:
    return {"forecast": f"sunny in {city}"}


@unittest.skipUnless(FARM, SKIP)
class AdkFarmContractTest(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = TemporaryDirectory()
        backend = load_backend("stub")
        backend.configure({"target": {"tool:get_weather": {}}})
        self.discovery = Discovery(Path(self._tmp.name))
        _runtime.set_runtime(backend, self.discovery)
        adk_pack.install()

    def tearDown(self) -> None:
        adk_pack.uninstall()
        _runtime.clear_runtime()
        self.discovery.close()
        self._tmp.cleanup()

    def _agent(self) -> "BaseAgent":
        # The real Runner._resolve_app requires one of app/agent/node — unlike
        # the structural fake, `agent=None` is rejected (module docs).
        return BaseAgent(name="farm_agent")

    def test_detect_reports_pinned_on_the_installed_version(self) -> None:
        det = adk_pack.detect()
        self.assertTrue(det.matched)
        self.assertEqual(det.confidence, "pinned", f"real version {det.version} fell out of range")

    def test_runner_chokepoint_and_plugin_manager_api(self) -> None:
        runner = InMemoryRunner(agent=self._agent(), app_name="farm")
        manager = runner.plugin_manager
        self.assertTrue(callable(manager.register_plugin) and callable(manager.get_plugin))
        plugin = manager.get_plugin(adk_pack.PLUGIN_NAME)
        self.assertIsNotNone(plugin, "auto-registration through the real Runner.__init__")
        self.assertIsInstance(plugin, BasePlugin)

    def test_install_uninstall_round_trips_on_the_real_runner(self) -> None:
        adk_pack.uninstall()
        pristine = Runner.__init__
        adk_pack.install()
        self.assertIsNot(Runner.__init__, pristine)
        adk_pack.uninstall()
        self.assertIs(Runner.__init__, pristine)
        adk_pack.install()  # leave installed for tearDown symmetry

    def test_rebind_preserves_real_plugin_manager_callback_sequencing(self) -> None:
        # The WS1 carry-forward: prove on the REAL PluginManager that keel's
        # before_tool_callback returning None lets a SECOND plugin's
        # before_tool_callback still run (early-exit only on non-None).
        runner = InMemoryRunner(agent=self._agent(), app_name="farm")
        manager = runner.plugin_manager
        seen: list[str] = []

        class Probe(BasePlugin):
            def __init__(self) -> None:
                super().__init__(name="probe")

            async def before_tool_callback(self, *, tool, tool_args, tool_context):
                seen.append(tool.name)
                return None

        manager.register_plugin(Probe())
        tool = FunctionTool(func=get_weather)
        result = asyncio.run(
            manager.run_before_tool_callback(tool=tool, tool_args={"city": "nyc"}, tool_context=None)
        )
        self.assertIsNone(result, "keel returned None -> real manager kept iterating")
        self.assertEqual(seen, [tool.name], "later plugin's callback still ran")
        self.assertTrue(
            getattr(tool.run_async, adk_pack._REBOUND_ATTR, False),
            "keel rebound the real FunctionTool on first sight",
        )
        # The real call now dispatches through Keel's wrapper (below the
        # plugin loop, per module docs) — confirm discovery sees it.
        out = asyncio.run(tool.run_async(args={"city": "nyc"}, tool_context=None))
        self.assertEqual(out, {"forecast": "sunny in nyc"})
        stats = _runtime.get_backend().report()["targets"]["tool:get_weather"]
        self.assertEqual(stats["successes"], 1)

    def test_mcp_tool_shape_assumptions(self) -> None:
        from google.adk.tools.mcp_tool.mcp_tool import McpTool

        self.assertIn("McpTool", [c.__name__ for c in McpTool.__mro__])
        fake = type("X", (McpTool,), {})  # subclass detection via MRO name
        self.assertTrue(adk_pack._is_mcp_tool(object.__new__(fake)))
        self.assertTrue(adk_pack._is_mcp_error_dict({"error": "boom"}))

    # -- WS5 Task 5: on_model_error real-registry + real-PluginManager certs
    # (5b) — the real-package counterparts of test_packs_adk.py's
    # ModelFallbackTest/ModelFallbackPluginShapeTest (which run entirely
    # against FakeLLMRegistry). -------------------------------------------

    def test_llm_registry_new_llm_constructs_a_real_gemini_instance(self) -> None:
        """5b cert: ``_model_fallback``'s chain-entry construction call —
        ``LLMRegistry.new_llm(entry)`` — actually builds a real provider
        model for a real registered name. This is the exact call
        ``adk_pack._model_fallback`` makes for a same-provider (Gemini)
        chain entry (before the same-class skip below it decides whether to
        use the result)."""
        from google.adk.models.google_llm import Gemini

        model = LLMRegistry.new_llm("gemini-2.0-flash")
        self.assertIsInstance(model, Gemini)
        self.assertEqual(model.model, "gemini-2.0-flash")

    def test_claude_name_without_anthropic_raises_actionable_error_our_skip_swallows(self) -> None:
        """5b cert: this farm venv has google-adk but deliberately NO
        anthropic package installed (module docstring) — resolving a
        claude-family name against the REAL LLMRegistry hits exactly the
        resolution-failure shape ``adk_pack._resolve_model_class``'s
        ``note_on_failure=True`` branch exists for: a real ``ValueError``
        carrying ADK's own actionable "requires the anthropic package"
        remedy. ``_model_fallback`` treats this as a skip-and-note
        chain-entry failure, not a crash — certified directly against our
        own helper below, fed the real registry."""
        claude_name = "claude-sonnet-cert-4-5"  # matches the real `claude-.*-4.*` regex
        with self.assertRaises(ValueError) as cm:
            LLMRegistry.resolve(claude_name)
        self.assertIn("anthropic", str(cm.exception).lower())

        # Our own helper swallows exactly this failure into a skip, not a crash.
        self.assertIsNone(adk_pack._resolve_model_class(LLMRegistry, claude_name, note_on_failure=True))

    def test_on_model_error_callback_drives_real_fallback_and_early_exits(self) -> None:
        """5b behavioral cert: registers a REAL provider model into the REAL
        LLMRegistry, then drives a REAL
        ``PluginManager.run_on_model_error_callback`` end-to-end — proving
        both that Keel's ``on_model_error_callback`` is reachable through
        ADK's real dispatch path, and that ``PluginManager``'s real
        early-exit-on-non-``None`` behavior (the WS1 carry-forward claim,
        here for the model-error hook rather than ``before_tool_callback``)
        actually fires: a later-registered probe plugin's own
        ``on_model_error_callback`` never runs."""
        fallback_calls: list[str] = []
        fallback_name = "keel-farm-cert-fallback-model"

        class _FarmFallbackModel(BaseLlm):
            @classmethod
            def supported_models(cls) -> list[str]:
                return [fallback_name]

            async def generate_content_async(self, llm_request: Any, stream: bool = False):
                fallback_calls.append(self.model)
                yield LlmResponse(
                    content=types.Content(role="model", parts=[types.Part(text="fallback-answer")]),
                    partial=False,
                )

        LLMRegistry.register(_FarmFallbackModel)
        try:
            backend = _runtime.get_backend()
            backend.configure({"target": {"llm:google-genai": {"fallback": [fallback_name]}}})

            runner = InMemoryRunner(agent=self._agent(), app_name="farm-model-fallback")
            manager = runner.plugin_manager  # keel's plugin auto-registered by Runner.__init__

            probe_seen: list[str] = []

            class Probe(BasePlugin):
                def __init__(self) -> None:
                    super().__init__(name="probe")

                async def on_model_error_callback(self, *, callback_context, llm_request, error):
                    probe_seen.append("probe-ran")
                    return None

            manager.register_plugin(Probe())

            llm_request = LlmRequest(model="gemini-2.0-flash")  # the ORIGINAL failing model
            result = asyncio.run(
                manager.run_on_model_error_callback(
                    callback_context=None, llm_request=llm_request, error=RuntimeError("boom")
                )
            )
            self.assertIsInstance(result, LlmResponse)
            self.assertEqual(
                fallback_calls, [fallback_name], "the real fallback model was actually constructed and driven"
            )
            self.assertEqual(
                probe_seen,
                [],
                "keel's non-None return short-circuited the real PluginManager before the probe ran",
            )
        finally:
            from google.adk.models import registry as registry_module

            registry_module._llm_registry_dict.pop(fallback_name, None)
            LLMRegistry.resolve.cache_clear()

    # -- WS5 Task 3: structural certs for the Runner-flow wrap (farm-only,
    # NO native module requirement on this leg — see module docs above) -----

    def test_run_async_signature_is_kw_only_and_carries_invocation_id(self) -> None:
        """Structural cert: the REAL Runner.run_async's public signature still
        matches what fixtures/fake_adk.py's FakeRunner (and adk_pack's own
        `_run_async_wrapper`, which binds by keyword) assume — every one of
        `user_id`/`session_id`/`invocation_id`/`new_message` is keyword-only,
        and `invocation_id` exists at all. Fences the offline fake against
        upstream ADK signature drift; this is a pure `inspect.signature` check,
        no flow/journal ever touched, so it needs no native module."""
        sig = inspect.signature(Runner.run_async)
        params = sig.parameters
        for name in ("user_id", "session_id", "invocation_id", "new_message"):
            self.assertIn(name, params, f"Runner.run_async lost its {name!r} parameter")
            self.assertEqual(
                params[name].kind,
                inspect.Parameter.KEYWORD_ONLY,
                f"Runner.run_async.{name} must stay keyword-only",
            )
        self.assertIsNone(
            params["invocation_id"].default, "invocation_id still defaults to None when omitted"
        )

    def test_run_async_is_transparent_when_undesignated_on_a_real_agent_loop(self) -> None:
        """Patched-generator transparency: this class's `setUp` never
        designates `RUNNER_FLOW_ENTRYPOINT` (no `[flows]` entrypoint is ever
        set up here), so the installed `Runner.run_async` wrapper must take
        its pass-through branch for a REAL agent turn — real ADK events flow
        through the patched async generator unaltered, and the STUB backend
        (which exposes no `enter_flow`/`exit_flow` at all) is never touched:
        if the wrapper mistakenly tried to open a flow on this undesignated
        call, `backend.enter_flow(...)` would raise `AttributeError` and this
        test would fail loudly rather than silently pass. No native module
        needed — this is Tier 0/pass-through, never Tier 2."""

        class _ScriptedTextModel(BaseLlm):
            model: str = "farm-transparency-model"
            turn: int = 0

            async def generate_content_async(self, llm_request: Any, stream: bool = False):
                self.turn += 1
                part = types.Part(text=f"turn-{self.turn}")
                yield LlmResponse(content=types.Content(role="model", parts=[part]), partial=False)

        model = _ScriptedTextModel()
        agent = LlmAgent(name="transparent_agent", model=model)
        runner = InMemoryRunner(agent=agent, app_name="farm-transparent")

        async def drive() -> list[Any]:
            session = await runner.session_service.create_session(
                app_name="farm-transparent", user_id="u1"
            )
            return [
                event
                async for event in runner.run_async(
                    user_id="u1",
                    session_id=session.id,
                    new_message=types.Content(role="user", parts=[types.Part(text="hi")]),
                )
            ]

        events = asyncio.run(asyncio.wait_for(drive(), timeout=15))
        self.assertTrue(events, "the undesignated real agent loop still produced events")
        self.assertEqual(model.turn, 1, "exactly one real model turn, undisturbed by the patch")


if __name__ == "__main__":
    unittest.main()
