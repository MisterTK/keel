"""The CrewAI framework pack (dx-spec §4.2): the adapter-pack four
operations, detection against a faked distribution, and the physical seam
(``CrewStructuredTool.invoke``/``.ainvoke``) wrapped/unwrapped reversibly
against a structural fake of the structured-tool API (real ``crewai`` is NOT
a repo dependency — CLAUDE.md: framework deps never get added to a manifest).

The fake ``CrewStructuredTool`` mirrors the real 1.15.2
``invoke(self, input, config=None, **kwargs) -> Any`` /
``async ainvoke(self, input, config=None, **kwargs) -> Any`` signatures
exactly (verified against the real package in a throwaway scratch venv while
building this pack; unlike the OpenAI Agents SDK's ``on_invoke_tool``, these
do NOT catch the wrapped function's own exception, so the fake's plain
try-nothing dispatch to ``self.func`` is faithful, not a simplification)."""

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
from keel.packs import crewai_pack as pack


def _fake_spec(name: str) -> importlib.machinery.ModuleSpec:
    return importlib.machinery.ModuleSpec(name, loader=None)


class _FakeCrewAI:
    """Installs (and tears down) a structural fake of
    ``crewai.tools.structured_tool.CrewStructuredTool`` in ``sys.modules``,
    plus a fake ``importlib.metadata.version`` for the ``crewai``
    distribution."""

    def __init__(self, version: str = "1.15.2") -> None:
        self.version = version
        self._patches: list[Any] = []

    def __enter__(self) -> type:
        crewai_pkg = types.ModuleType("crewai")
        crewai_pkg.__spec__ = _fake_spec("crewai")
        crewai_pkg.__path__ = []
        sys.modules["crewai"] = crewai_pkg

        tools_pkg = types.ModuleType("crewai.tools")
        tools_pkg.__spec__ = _fake_spec("crewai.tools")
        tools_pkg.__path__ = []
        sys.modules["crewai.tools"] = tools_pkg
        crewai_pkg.tools = tools_pkg  # type: ignore[attr-defined]

        structured_mod = types.ModuleType("crewai.tools.structured_tool")
        structured_mod.__spec__ = _fake_spec("crewai.tools.structured_tool")

        class CrewStructuredTool:
            def __init__(self, name: str, func: Callable[..., Any]) -> None:
                self.name = name
                self.func = func

            def invoke(self, input: Any, config: Any = None, **kwargs: Any) -> Any:
                args = input if isinstance(input, dict) else {}
                return self.func(**args)

            async def ainvoke(self, input: Any, config: Any = None, **kwargs: Any) -> Any:
                args = input if isinstance(input, dict) else {}
                fn = self.func
                if inspect.iscoroutinefunction(fn):
                    return await fn(**args)
                return fn(**args)

        structured_mod.CrewStructuredTool = CrewStructuredTool  # type: ignore[attr-defined]
        sys.modules["crewai.tools.structured_tool"] = structured_mod
        tools_pkg.structured_tool = structured_mod  # type: ignore[attr-defined]

        def fake_version(dist: str) -> str:
            if dist == "crewai":
                return self.version
            raise __import__("importlib.metadata", fromlist=["PackageNotFoundError"]).PackageNotFoundError(dist)

        p = mock.patch("importlib.metadata.version", side_effect=fake_version)
        p.start()
        self._patches.append(p)
        return CrewStructuredTool

    def __exit__(self, *exc: Any) -> None:
        pack.uninstall()
        for p in self._patches:
            p.stop()
        for name in ("crewai.tools.structured_tool", "crewai.tools", "crewai"):
            sys.modules.pop(name, None)


class ContractShapeTest(unittest.TestCase):
    def test_seams_targets_defaults(self) -> None:
        seams = pack.seams()
        self.assertEqual(len(seams), 1)
        self.assertIn("CrewStructuredTool.invoke", seams[0].patch_point)
        self.assertIn("ainvoke", seams[0].patch_point)
        targets = pack.targets()
        self.assertEqual(targets[0].pattern, "tool:<name>")
        self.assertEqual(targets[0].kind, "tool")
        self.assertIn("idempotent=False", targets[0].idempotency_rule)
        self.assertEqual(targets[1].pattern, "llm:<provider>")
        self.assertEqual(targets[1].kind, "llm")
        self.assertEqual(pack.defaults(), {})

    def test_not_installed_in_this_repo_venv(self) -> None:
        self.assertFalse(pack.detect().matched)


class DetectTest(unittest.TestCase):
    def test_pinned_version_reports_pinned(self) -> None:
        with _FakeCrewAI("1.15.2"):
            d = pack.detect()
        self.assertTrue(d.matched)
        self.assertEqual(d.name, "crewai")
        self.assertEqual(d.version, "1.15.2")
        self.assertEqual(d.confidence, "pinned")

    def test_version_2_is_best_effort(self) -> None:
        with _FakeCrewAI("2.0.0"):
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
    def test_invoke_and_ainvoke_are_patched_and_restored(self) -> None:
        with _FakeCrewAI() as CrewStructuredTool:
            orig_invoke = CrewStructuredTool.invoke
            orig_ainvoke = CrewStructuredTool.ainvoke
            pack.install()
            self.assertIsNot(CrewStructuredTool.invoke, orig_invoke)
            self.assertIsNot(CrewStructuredTool.ainvoke, orig_ainvoke)
            self.assertTrue(getattr(CrewStructuredTool.invoke, "__keel_wrapped__", False))
            self.assertTrue(getattr(CrewStructuredTool.ainvoke, "__keel_wrapped__", False))
            pack.uninstall()
            self.assertIs(CrewStructuredTool.invoke, orig_invoke)
            self.assertIs(CrewStructuredTool.ainvoke, orig_ainvoke)

    def test_install_is_idempotent(self) -> None:
        with _FakeCrewAI() as CrewStructuredTool:
            pack.install()
            patched_invoke = CrewStructuredTool.invoke
            patched_ainvoke = CrewStructuredTool.ainvoke
            pack.install()
            self.assertIs(CrewStructuredTool.invoke, patched_invoke)
            self.assertIs(CrewStructuredTool.ainvoke, patched_ainvoke)
            pack.uninstall()

    def test_install_noop_when_absent(self) -> None:
        for name in ("crewai.tools.structured_tool", "crewai.tools", "crewai"):
            sys.modules.pop(name, None)
        pack.install()
        self.assertFalse(pack._installed)  # noqa: SLF001 (white-box on purpose)


class NonIdempotentDefaultTest(InstallUninstallTestBase):
    def test_sync_conn_error_observed_not_retried_e014(self) -> None:
        self.use_backend(
            {"target": {"tool:charge_card": {"retry": {"attempts": 3, "on": ["conn"], "schedule": "fixed(1ms)"}}}}
        )
        with _FakeCrewAI() as CrewStructuredTool:
            pack.install()
            calls = {"n": 0}
            original = ConnectionError("reset")

            def charge(**_kwargs: Any) -> None:
                calls["n"] += 1
                raise original

            tool = CrewStructuredTool(name="charge_card", func=charge)
            with self.assertRaises(ConnectionError) as ctx:
                tool.invoke({})
            self.assertIs(ctx.exception, original)
            self.assertEqual(calls["n"], 1, "a framework tool is NOT retried by default")
            self.assertEqual(ctx.exception.keel_outcome["error"]["code"], "KEEL-E014")
            pack.uninstall()

    def test_async_conn_error_observed_not_retried_e014(self) -> None:
        self.use_backend(
            {"target": {"tool:post_msg": {"retry": {"attempts": 3, "on": ["conn"], "schedule": "fixed(1ms)"}}}}
        )
        with _FakeCrewAI() as CrewStructuredTool:
            pack.install()
            calls = {"n": 0}
            original = ConnectionError("reset")

            async def post(**_kwargs: Any) -> None:
                calls["n"] += 1
                raise original

            tool = CrewStructuredTool(name="post_msg", func=post)
            with self.assertRaises(ConnectionError) as ctx:
                asyncio.run(tool.ainvoke({}))
            self.assertIs(ctx.exception, original)
            self.assertEqual(calls["n"], 1)
            self.assertEqual(ctx.exception.keel_outcome["error"]["code"], "KEEL-E014")
            pack.uninstall()

    def test_sync_success_returns_live_result(self) -> None:
        self.use_backend(apply_pack_defaults({}))
        with _FakeCrewAI() as CrewStructuredTool:
            pack.install()
            sentinel = {"forecast": "sunny"}
            tool = CrewStructuredTool(name="get_weather", func=lambda **_kw: sentinel)
            result = tool.invoke({"city": "Paris"})
            self.assertIs(result, sentinel)
            pack.uninstall()

    def test_async_success_returns_live_result(self) -> None:
        self.use_backend(apply_pack_defaults({}))
        with _FakeCrewAI() as CrewStructuredTool:
            pack.install()
            sentinel = {"forecast": "sunny"}

            async def get_weather(**_kwargs: Any) -> dict[str, Any]:
                return sentinel

            tool = CrewStructuredTool(name="get_weather", func=get_weather)
            result = asyncio.run(tool.ainvoke({"city": "Paris"}))
            self.assertIs(result, sentinel)
            pack.uninstall()


class SkippedNameTest(InstallUninstallTestBase):
    def test_invalid_tool_name_passes_through_sync_and_is_recorded(self) -> None:
        self.use_backend(apply_pack_defaults({}))
        with _FakeCrewAI() as CrewStructuredTool:
            pack.install()
            calls = {"n": 0}

            def delegate(**_kwargs: Any) -> str:
                calls["n"] += 1
                return "delegated"

            tool = CrewStructuredTool(name="Delegate work to coworker", func=delegate)
            result = tool.invoke({})
            self.assertEqual(result, "delegated")
            self.assertEqual(calls["n"], 1)
            self.assertIn("Delegate work to coworker", pack.SKIPPED)
            pack.uninstall()

    def test_invalid_tool_name_passes_through_async(self) -> None:
        self.use_backend(apply_pack_defaults({}))
        with _FakeCrewAI() as CrewStructuredTool:
            pack.install()

            async def ask(**_kwargs: Any) -> str:
                return "asked"

            tool = CrewStructuredTool(name="Ask question to coworker", func=ask)
            result = asyncio.run(tool.ainvoke({}))
            self.assertEqual(result, "asked")
            self.assertIn("Ask question to coworker", pack.SKIPPED)
            pack.uninstall()


class DiscoveryTest(InstallUninstallTestBase):
    def test_sync_and_async_paths_use_separate_runner_caches(self) -> None:
        # The same tool name dispatched via BOTH invoke and ainvoke must not
        # collide (a sync wrapper is never interchangeable with an async one).
        backend = self.use_backend(apply_pack_defaults({}))
        with _FakeCrewAI() as CrewStructuredTool:
            pack.install()

            def sync_fn(**_kw: Any) -> str:
                return "sync"

            async def async_fn(**_kw: Any) -> str:
                return "async"

            sync_tool = CrewStructuredTool(name="lookup", func=sync_fn)
            async_tool = CrewStructuredTool(name="lookup", func=async_fn)
            self.assertEqual(sync_tool.invoke({}), "sync")
            self.assertEqual(asyncio.run(async_tool.ainvoke({})), "async")
            stats = backend.report()["targets"]["tool:lookup"]
            self.assertEqual(stats["successes"], 2)
            pack.uninstall()


if __name__ == "__main__":
    unittest.main()
