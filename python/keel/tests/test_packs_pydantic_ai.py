"""The Pydantic AI framework pack (dx-spec §4.2): the adapter-pack four
operations, detection against a faked distribution, and the physical seam
(``FunctionToolset.call_tool``) wrapped/unwrapped reversibly against a
structural fake of the toolsets API (real pydantic-ai is NOT a repo
dependency — CLAUDE.md: framework deps never get added to a manifest).

The fake mirrors the real 2.9.0 ``call_tool(self, name, tool_args, ctx,
tool) -> Any`` signature exactly (verified against the real package in a
throwaway scratch venv while building this pack) and dispatches to ``tool``
as the underlying callable, which is close enough to the real toolset's
behavior for every judgment this pack makes (name -> target, idempotent
default, args_hash, SKIPPED bookkeeping) without requiring the framework's
full machinery.
"""

from __future__ import annotations

import asyncio
import importlib.machinery
import inspect
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
from keel.packs import pydantic_ai_pack as pack


def _fake_spec(name: str) -> importlib.machinery.ModuleSpec:
    return importlib.machinery.ModuleSpec(name, loader=None)


class _FakePydanticAI:
    """Installs (and tears down) a structural fake of ``pydantic_ai`` +
    ``pydantic_ai.toolsets.FunctionToolset`` in ``sys.modules``, and a fake
    ``importlib.metadata.version`` so ``detect()`` sees a chosen version."""

    def __init__(self, version: str = "2.9.0") -> None:
        self.version = version
        self._patches: list[Any] = []

    def __enter__(self) -> type:
        pkg = types.ModuleType("pydantic_ai")
        pkg.__spec__ = _fake_spec("pydantic_ai")
        pkg.__path__ = []  # a package, so `pydantic_ai.toolsets` resolves
        sys.modules["pydantic_ai"] = pkg

        toolsets_mod = types.ModuleType("pydantic_ai.toolsets")
        toolsets_mod.__spec__ = _fake_spec("pydantic_ai.toolsets")

        class FunctionToolset:
            async def call_tool(
                self, name: str, tool_args: dict[str, Any], ctx: Any, tool: Callable[..., Any]
            ) -> Any:
                if inspect.iscoroutinefunction(tool):
                    return await tool(**tool_args)
                return tool(**tool_args)

        toolsets_mod.FunctionToolset = FunctionToolset  # type: ignore[attr-defined]
        sys.modules["pydantic_ai.toolsets"] = toolsets_mod
        pkg.toolsets = toolsets_mod  # type: ignore[attr-defined]

        def fake_version(dist: str) -> str:
            if dist == "pydantic-ai-slim":
                return self.version
            raise __import__("importlib.metadata", fromlist=["PackageNotFoundError"]).PackageNotFoundError(dist)

        p = mock.patch("importlib.metadata.version", side_effect=fake_version)
        p.start()
        self._patches.append(p)
        return FunctionToolset

    def __exit__(self, *exc: Any) -> None:
        pack.uninstall()
        for p in self._patches:
            p.stop()
        for name in ("pydantic_ai.toolsets", "pydantic_ai"):
            sys.modules.pop(name, None)


class ContractShapeTest(unittest.TestCase):
    """The four operations, independent of whether pydantic-ai is present."""

    def test_seams_targets_defaults(self) -> None:
        seams = pack.seams()
        self.assertEqual(len(seams), 1)
        self.assertIn("FunctionToolset.call_tool", seams[0].patch_point)
        targets = pack.targets()
        self.assertEqual(targets[0].pattern, "tool:<name>")
        self.assertEqual(targets[0].kind, "tool")
        self.assertIn("idempotent=False", targets[0].idempotency_rule)
        self.assertEqual(targets[1].pattern, "llm:<provider>")
        self.assertEqual(targets[1].kind, "llm")
        self.assertEqual(pack.defaults(), {})

    def test_not_installed_in_this_repo_venv(self) -> None:
        # CLAUDE.md: framework deps are never added to a repo manifest.
        self.assertFalse(pack.detect().matched)


class DetectTest(unittest.TestCase):
    def test_pinned_version_reports_pinned(self) -> None:
        with _FakePydanticAI("2.9.0"):
            d = pack.detect()
        self.assertTrue(d.matched)
        self.assertEqual(d.name, "pydantic-ai")
        self.assertEqual(d.version, "2.9.0")
        self.assertEqual(d.confidence, "pinned")

    def test_newer_major_is_best_effort(self) -> None:
        with _FakePydanticAI("3.0.0"):
            d = pack.detect()
        self.assertTrue(d.matched)
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
    def test_call_tool_class_is_patched_and_restored(self) -> None:
        with _FakePydanticAI() as FunctionToolset:
            orig = FunctionToolset.call_tool
            pack.install()
            self.assertIsNot(FunctionToolset.call_tool, orig)
            self.assertTrue(getattr(FunctionToolset.call_tool, "__keel_wrapped__", False))
            pack.uninstall()
            self.assertIs(FunctionToolset.call_tool, orig)

    def test_install_is_idempotent(self) -> None:
        with _FakePydanticAI() as FunctionToolset:
            pack.install()
            patched = FunctionToolset.call_tool
            pack.install()  # second call: no-op, same wrapper
            self.assertIs(FunctionToolset.call_tool, patched)
            pack.uninstall()

    def test_install_noop_when_absent(self) -> None:
        sys.modules.pop("pydantic_ai", None)
        sys.modules.pop("pydantic_ai.toolsets", None)
        pack.install()  # ImportError inside install(): swallowed, no state
        self.assertFalse(pack._installed)  # noqa: SLF001 (white-box on purpose)


class NonIdempotentDefaultTest(InstallUninstallTestBase):
    def test_conn_error_observed_not_retried_e014(self) -> None:
        self.use_backend(
            {"target": {"tool:charge_card": {"retry": {"attempts": 3, "on": ["conn"], "schedule": "fixed(1ms)"}}}}
        )
        with _FakePydanticAI() as FunctionToolset:
            pack.install()
            toolset = FunctionToolset()
            calls = {"n": 0}
            original = ConnectionError("reset")

            def charge(**_kwargs: Any) -> None:
                calls["n"] += 1
                raise original

            with self.assertRaises(ConnectionError) as ctx:
                asyncio.run(toolset.call_tool("charge_card", {}, None, charge))
            self.assertIs(ctx.exception, original)
            self.assertEqual(calls["n"], 1, "a framework tool is NOT retried by default")
            self.assertEqual(ctx.exception.keel_outcome["error"]["code"], "KEEL-E014")
            pack.uninstall()

    def test_success_returns_live_result(self) -> None:
        self.use_backend(apply_pack_defaults({}))
        with _FakePydanticAI() as FunctionToolset:
            pack.install()
            toolset = FunctionToolset()
            sentinel = {"nested": (1, 2)}

            async def get_weather(**_kwargs: Any) -> dict[str, Any]:
                return sentinel

            result = asyncio.run(toolset.call_tool("get_weather", {"city": "Paris"}, None, get_weather))
            self.assertIs(result, sentinel)
            pack.uninstall()


class SkippedNameTest(InstallUninstallTestBase):
    def test_invalid_tool_name_passes_through_and_is_recorded(self) -> None:
        self.use_backend(apply_pack_defaults({}))
        with _FakePydanticAI() as FunctionToolset:
            pack.install()
            toolset = FunctionToolset()
            calls = {"n": 0}

            async def weird(**_kwargs: Any) -> str:
                calls["n"] += 1
                return "ok"

            result = asyncio.run(toolset.call_tool("get weather", {}, None, weird))
            self.assertEqual(result, "ok")
            self.assertEqual(calls["n"], 1)
            self.assertIn("get weather", pack.SKIPPED)
            pack.uninstall()


class DiscoveryTest(InstallUninstallTestBase):
    def test_target_key_and_reuse_across_calls(self) -> None:
        backend = self.use_backend(apply_pack_defaults({}))
        with _FakePydanticAI() as FunctionToolset:
            pack.install()
            toolset = FunctionToolset()

            async def lookup(**_kwargs: Any) -> str:
                return "found"

            asyncio.run(toolset.call_tool("lookup", {}, None, lookup))
            asyncio.run(toolset.call_tool("lookup", {}, None, lookup))
            stats = backend.report()["targets"]["tool:lookup"]
            self.assertEqual(stats["successes"], 2)
            pack.uninstall()


if __name__ == "__main__":
    unittest.main()
