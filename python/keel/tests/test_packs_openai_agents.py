"""The OpenAI Agents SDK framework pack (dx-spec §4.2): the adapter-pack four
operations, detection against a faked distribution, and the physical seam
(``FunctionTool.__post_init__`` wrapping ``on_invoke_tool`` per instance)
wrapped/unwrapped reversibly against a structural fake of the SDK's tool
dataclass (real ``agents`` is NOT a repo dependency — CLAUDE.md: framework
deps never get added to a manifest).

The fake ``FunctionTool`` mirrors the real 0.18.2 dataclass shape exactly
(``name``, ``on_invoke_tool``, a ``__post_init__`` hook — verified against the
real package in a throwaway scratch venv while building this pack), which is
everything :func:`keel.packs.openai_agents_pack.install` touches.
"""

from __future__ import annotations

import asyncio
import dataclasses
import importlib.machinery
import sys
import types
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory
from typing import Any, Callable
from unittest import mock

from keel import _runtime
from keel._backend import load_backend
from keel._defaults import apply_pack_defaults
from keel._discovery import Discovery
from keel.packs import openai_agents_pack as pack


def _fake_spec(name: str) -> importlib.machinery.ModuleSpec:
    return importlib.machinery.ModuleSpec(name, loader=None)


class _FakeAgentsSDK:
    """Installs (and tears down) a structural fake of the ``agents`` module
    (``FunctionTool`` dataclass only) in ``sys.modules``, plus a fake
    ``importlib.metadata.version`` for the ``openai-agents`` distribution."""

    def __init__(self, version: str = "0.18.2") -> None:
        self.version = version
        self._patches: list[Any] = []

    def __enter__(self) -> type:
        mod = types.ModuleType("agents")
        mod.__spec__ = _fake_spec("agents")

        @dataclasses.dataclass
        class FunctionTool:
            name: str
            on_invoke_tool: Callable[..., Any]

            def __post_init__(self) -> None:
                pass  # the real dataclass does JSON-schema strictness work here

        mod.FunctionTool = FunctionTool  # type: ignore[attr-defined]
        sys.modules["agents"] = mod

        def fake_version(dist: str) -> str:
            if dist == "openai-agents":
                return self.version
            raise __import__("importlib.metadata", fromlist=["PackageNotFoundError"]).PackageNotFoundError(dist)

        p = mock.patch("importlib.metadata.version", side_effect=fake_version)
        p.start()
        self._patches.append(p)
        return FunctionTool

    def __exit__(self, *exc: Any) -> None:
        pack.uninstall()
        for p in self._patches:
            p.stop()
        sys.modules.pop("agents", None)


class ContractShapeTest(unittest.TestCase):
    def test_seams_targets_defaults(self) -> None:
        seams = pack.seams()
        self.assertEqual(len(seams), 1)
        self.assertIn("__post_init__", seams[0].patch_point)
        self.assertIn("on_invoke_tool", seams[0].upstream_api)
        targets = pack.targets()
        self.assertEqual(targets[0].pattern, "tool:<name>")
        self.assertEqual(targets[0].kind, "tool")
        self.assertIn("idempotent=False", targets[0].idempotency_rule)
        self.assertEqual(targets[1].pattern, "llm:openai")
        self.assertEqual(targets[1].kind, "llm")
        self.assertEqual(pack.defaults(), {})

    def test_not_installed_in_this_repo_venv(self) -> None:
        self.assertFalse(pack.detect().matched)


class DetectTest(unittest.TestCase):
    def test_requires_the_distribution_not_just_the_module(self) -> None:
        # A bare `agents` module with no distribution metadata is NOT proof of
        # the OpenAI Agents SDK (require_dist=True: too generic a name).
        mod = types.ModuleType("agents")
        mod.__spec__ = _fake_spec("agents")
        sys.modules["agents"] = mod
        try:
            self.assertFalse(pack.detect().matched)
        finally:
            sys.modules.pop("agents", None)

    def test_pinned_version_reports_pinned(self) -> None:
        with _FakeAgentsSDK("0.18.2"):
            d = pack.detect()
        self.assertTrue(d.matched)
        self.assertEqual(d.name, "openai-agents")
        self.assertEqual(d.version, "0.18.2")
        self.assertEqual(d.confidence, "pinned")

    def test_version_1_is_best_effort(self) -> None:
        with _FakeAgentsSDK("1.0.0"):
            d = pack.detect()
        self.assertEqual(d.confidence, "best_effort")


class InstallUninstallTestBase(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = TemporaryDirectory()
        self.cwd = Path(self._tmp.name)

    def tearDown(self) -> None:
        _runtime.clear_runtime()
        self._tmp.cleanup()

    def use_backend(self, policy: dict[str, Any]) -> Any:
        backend = load_backend("stub")
        backend.configure(policy)
        discovery = Discovery(self.cwd)
        self.addCleanup(discovery.close)
        _runtime.set_runtime(backend, discovery)
        return backend


class SeamWrappingTest(InstallUninstallTestBase):
    def test_post_init_is_patched_and_restored(self) -> None:
        with _FakeAgentsSDK() as FunctionTool:
            orig_post_init = FunctionTool.__post_init__
            pack.install()
            self.assertIsNot(FunctionTool.__post_init__, orig_post_init)

            async def invoke(_ctx: Any, _args: str) -> str:
                return "ok"

            tool = FunctionTool(name="lookup", on_invoke_tool=invoke)
            self.assertTrue(getattr(tool.on_invoke_tool, "__keel_wrapped__", False))
            pack.uninstall()
            self.assertIs(FunctionTool.__post_init__, orig_post_init)

    def test_install_is_idempotent(self) -> None:
        with _FakeAgentsSDK() as FunctionTool:
            pack.install()
            patched = FunctionTool.__post_init__
            pack.install()
            self.assertIs(FunctionTool.__post_init__, patched)
            pack.uninstall()

    def test_install_noop_when_absent(self) -> None:
        sys.modules.pop("agents", None)
        pack.install()
        self.assertFalse(pack._installed)  # noqa: SLF001 (white-box on purpose)


class NonIdempotentDefaultTest(InstallUninstallTestBase):
    def test_conn_error_observed_not_retried_e014(self) -> None:
        self.use_backend(
            {"target": {"tool:charge_card": {"retry": {"attempts": 3, "on": ["conn"], "schedule": "fixed(1ms)"}}}}
        )
        with _FakeAgentsSDK() as FunctionTool:
            pack.install()
            calls = {"n": 0}
            original = ConnectionError("reset")

            async def invoke(_ctx: Any, _args: str) -> None:
                calls["n"] += 1
                raise original

            tool = FunctionTool(name="charge_card", on_invoke_tool=invoke)
            with self.assertRaises(ConnectionError) as ctx:
                asyncio.run(tool.on_invoke_tool(None, "{}"))
            self.assertIs(ctx.exception, original)
            self.assertEqual(calls["n"], 1, "a framework tool is NOT retried by default")
            self.assertEqual(ctx.exception.keel_outcome["error"]["code"], "KEEL-E014")
            pack.uninstall()

    def test_success_returns_live_result(self) -> None:
        self.use_backend(apply_pack_defaults({}))
        with _FakeAgentsSDK() as FunctionTool:
            pack.install()
            sentinel = {"forecast": "sunny"}

            async def invoke(_ctx: Any, _args: str) -> dict[str, Any]:
                return sentinel

            tool = FunctionTool(name="get_weather", on_invoke_tool=invoke)
            result = asyncio.run(tool.on_invoke_tool(None, "{}"))
            self.assertIs(result, sentinel)
            pack.uninstall()


class SkippedNameTest(InstallUninstallTestBase):
    def test_invalid_tool_name_passes_through_and_is_recorded(self) -> None:
        self.use_backend(apply_pack_defaults({}))
        with _FakeAgentsSDK() as FunctionTool:
            pack.install()
            calls = {"n": 0}

            async def invoke(_ctx: Any, _args: str) -> str:
                calls["n"] += 1
                return "ok"

            tool = FunctionTool(name="Delegate work to coworker", on_invoke_tool=invoke)
            # No __keel_wrapped__: an unroutable name is passed through untouched.
            self.assertFalse(getattr(tool.on_invoke_tool, "__keel_wrapped__", False))
            result = asyncio.run(tool.on_invoke_tool(None, "{}"))
            self.assertEqual(result, "ok")
            self.assertEqual(calls["n"], 1)
            self.assertIn("Delegate work to coworker", pack.SKIPPED)
            pack.uninstall()


class ReconstructionTest(InstallUninstallTestBase):
    def test_double_post_init_does_not_double_wrap(self) -> None:
        # dataclasses.replace()/copy() re-run __post_init__ on an ALREADY
        # wrapped on_invoke_tool; the WRAPPED_ATTR marker must short-circuit it.
        self.use_backend(apply_pack_defaults({}))
        with _FakeAgentsSDK() as FunctionTool:
            pack.install()
            calls = {"n": 0}

            async def invoke(_ctx: Any, _args: str) -> str:
                calls["n"] += 1
                return "ok"

            tool = FunctionTool(name="lookup", on_invoke_tool=invoke)
            once_wrapped = tool.on_invoke_tool
            copy = dataclasses.replace(tool)
            self.assertIs(copy.on_invoke_tool, once_wrapped, "not re-wrapped")
            pack.uninstall()


if __name__ == "__main__":
    unittest.main()
