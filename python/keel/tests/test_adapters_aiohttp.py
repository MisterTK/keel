"""aiohttp pack tests against a structural fake of aiohttp (aiohttp is not
installed in this environment and must never become a repo dependency — see
CLAUDE.md). The fake mirrors just the shapes ``aiohttp_pack`` touches:
``ClientSession._request``/``_build_url``, a response object exposing
``status``/``headers``/``read()``, and the ``ClientConnectionError``
exception. The design (seam choice, judgment, the ``_ReplayedResponse``
cache-hit facade) was verified against the REAL aiohttp 3.14 in a throwaway
venv during development; this fake reproduces those same observed shapes."""

from __future__ import annotations

import asyncio
import importlib.machinery
import sqlite3
import sys
import types
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory
from typing import Any
from urllib.parse import urlsplit

from keel import _runtime
from keel._backend import load_backend
from keel._defaults import level0_defaults
from keel._discovery import Discovery
from keel._errors import KeelError


# --- the structural fake ------------------------------------------------------


class _FakeClientError(Exception):
    pass


class _FakeClientConnectionError(_FakeClientError):
    pass


class _FakeHeaders:
    """A tiny case-insensitive mapping, mirroring aiohttp's CIMultiDictProxy
    enough for ``.get()``/``.items()``."""

    def __init__(self, items: dict[str, str] | None = None) -> None:
        self._items = list((items or {}).items())

    def get(self, key: str, default: Any = None) -> Any:
        lk = key.lower()
        for k, v in self._items:
            if k.lower() == lk:
                return v
        return default

    def items(self) -> list[tuple[str, str]]:
        return list(self._items)


class _FakeResponse:
    def __init__(self, status: int, body: bytes = b"", headers: dict[str, str] | None = None) -> None:
        self.status = status
        self.headers = _FakeHeaders(headers)
        self._body = body
        self.released = False

    async def read(self) -> bytes:
        return self._body

    def release(self) -> None:
        self.released = True

    def close(self) -> None:
        self.released = True


class _FakeURL:
    def __init__(self, url: str) -> None:
        parts = urlsplit(url)
        self.host = parts.hostname
        self.path = parts.path or "/"
        self._url = url

    def __str__(self) -> str:
        return self._url


class _ScriptedSession:
    """The seam target: ``_request`` is exactly what ``aiohttp_pack`` patches
    on the real ``aiohttp.ClientSession``. A test scripts a queue of
    responses/exceptions; ``get``/``post`` mirror the real convenience verbs
    (both funnel through ``self._request``, so once installed they exercise
    the WRAPPED version automatically)."""

    def __init__(self, script: list[Any], base_url: str | None = None) -> None:
        self._script = list(script)
        self._base_url = base_url
        self.served = 0

    def _build_url(self, str_or_url: Any) -> _FakeURL:
        if self._base_url and not str(str_or_url).startswith(("http://", "https://")):
            return _FakeURL(self._base_url.rstrip("/") + "/" + str(str_or_url).lstrip("/"))
        return _FakeURL(str(str_or_url))

    async def _request(self, method: str, url: Any, **kwargs: Any) -> Any:
        self.served += 1
        directive = self._script.pop(0) if self._script else _FakeResponse(200)
        if isinstance(directive, BaseException):
            raise directive
        return directive

    def get(self, url: Any, **kwargs: Any) -> "_RequestCtx":
        return _RequestCtx(self._request("GET", url, **kwargs))

    def post(self, url: Any, **kwargs: Any) -> "_RequestCtx":
        return _RequestCtx(self._request("POST", url, **kwargs))


class _RequestCtx:
    """Mirrors aiohttp's ``_RequestContextManager``: awaitable directly, and
    usable as ``async with session.get(url) as resp:``."""

    def __init__(self, coro: Any) -> None:
        self._coro = coro
        self._resp: Any = None

    def __await__(self) -> Any:
        return self._coro.__await__()

    async def __aenter__(self) -> Any:
        self._resp = await self._coro
        return self._resp

    async def __aexit__(self, *exc: Any) -> None:
        if self._resp is not None:
            self._resp.close()


def _install_fake_aiohttp() -> types.ModuleType:
    root = types.ModuleType("aiohttp")
    root.__spec__ = importlib.machinery.ModuleSpec("aiohttp", loader=None)
    root.ClientSession = _ScriptedSession
    root.ClientError = _FakeClientError
    root.ClientConnectionError = _FakeClientConnectionError
    sys.modules["aiohttp"] = root
    return root


def _uninstall_fake_aiohttp() -> None:
    sys.modules.pop("aiohttp", None)


# --- tests ---------------------------------------------------------------------


class AiohttpTestBase(unittest.TestCase):
    def setUp(self) -> None:
        _install_fake_aiohttp()
        self.addCleanup(_uninstall_fake_aiohttp)
        self._tmp = TemporaryDirectory()
        self.cwd = Path(self._tmp.name)
        self.backend = load_backend("stub")
        self.backend.configure(level0_defaults())
        self.discovery = Discovery(self.cwd)
        _runtime.set_runtime(self.backend, self.discovery)
        from keel.adapters import aiohttp_pack

        self.aiohttp_pack = aiohttp_pack
        aiohttp_pack.install()
        self.addCleanup(aiohttp_pack.uninstall)

    def tearDown(self) -> None:
        _runtime.clear_runtime()
        self.discovery.close()
        self._tmp.cleanup()

    def rows(self) -> dict[str, sqlite3.Row]:
        self.discovery.close()
        conn = sqlite3.connect(self.discovery.db_path)
        conn.row_factory = sqlite3.Row
        try:
            return {r["target"]: r for r in conn.execute("SELECT * FROM discovery")}
        finally:
            conn.close()

    def run_async(self, coro: Any) -> Any:
        return asyncio.run(coro)


class ContractTest(AiohttpTestBase):
    def test_detect_reports_aiohttp_present(self) -> None:
        d = self.aiohttp_pack.detect()
        self.assertTrue(d.matched)
        self.assertEqual(d.name, "aiohttp")

    def test_seams_targets_defaults(self) -> None:
        seams = self.aiohttp_pack.seams()
        self.assertEqual(seams[0].patch_point, "aiohttp.ClientSession._request")
        targets = self.aiohttp_pack.targets()
        self.assertEqual(targets[0].pattern, "<request host>")
        llm_patterns = {t.pattern for t in targets if t.kind == "llm"}
        self.assertIn("llm:openai", llm_patterns)
        self.assertEqual(self.aiohttp_pack.defaults(), {})

    def test_install_uninstall_reversible(self) -> None:
        import aiohttp

        self.assertTrue(getattr(aiohttp.ClientSession._request, "__keel_wrapped__", False))
        self.aiohttp_pack.uninstall()
        self.assertIs(aiohttp.ClientSession._request, _ScriptedSession._request)
        self.aiohttp_pack.install()  # restore for addCleanup symmetry


class TransparencyTest(AiohttpTestBase):
    def test_success_returns_the_live_response_unchanged(self) -> None:
        async def go() -> None:
            session = _ScriptedSession([_FakeResponse(200, b"hi")])
            async with session.get("http://127.0.0.1/p") as resp:
                self.assertIsInstance(resp, _FakeResponse)
                self.assertEqual(await resp.read(), b"hi")
                self.assertEqual(resp.keel_outcome["result"], "ok")

        self.run_async(go())


class ResilienceTest(AiohttpTestBase):
    def test_5xx_then_ok_is_retried(self) -> None:
        async def go() -> None:
            session = _ScriptedSession([_FakeResponse(503), _FakeResponse(200, b"recovered")])
            resp = await session.get("http://127.0.0.1/")
            self.assertEqual(resp.status, 200)
            self.assertEqual(await resp.read(), b"recovered")
            self.assertEqual(resp.keel_outcome["attempts"], 2)

        self.run_async(go())

    def test_429_retry_after_governs_backoff(self) -> None:
        async def go() -> None:
            session = _ScriptedSession(
                [_FakeResponse(429, headers={"Retry-After": "1"}), _FakeResponse(200, b"ok")]
            )
            resp = await session.get("http://127.0.0.1/")
            self.assertEqual(resp.status, 200)
            self.assertEqual(resp.keel_outcome["waits_ms"], [1000])

        self.run_async(go())

    def test_connection_error_is_retried(self) -> None:
        async def go() -> None:
            session = _ScriptedSession([_FakeClientConnectionError("reset"), _FakeResponse(200, b"after-reset")])
            resp = await session.get("http://127.0.0.1/")
            self.assertEqual(await resp.read(), b"after-reset")
            self.assertEqual(resp.keel_outcome["attempts"], 2)

        self.run_async(go())

    def test_timeout_error_is_retried(self) -> None:
        async def go() -> None:
            session = _ScriptedSession([TimeoutError("slow"), _FakeResponse(200, b"fast")])
            resp = await session.get("http://127.0.0.1/")
            self.assertEqual(await resp.read(), b"fast")
            self.assertEqual(resp.keel_outcome["attempts"], 2)

        self.run_async(go())

    def test_non_429_4xx_passes_through_unretried(self) -> None:
        async def go() -> None:
            session = _ScriptedSession([_FakeResponse(404, b"missing")])
            resp = await session.get("http://127.0.0.1/")
            self.assertEqual(resp.status, 404)
            self.assertEqual(resp.keel_outcome["result"], "ok")
            self.assertEqual(resp.keel_outcome["attempts"], 1)

        self.run_async(go())


class HardRulesTest(AiohttpTestBase):
    def test_post_without_key_is_observed_not_retried(self) -> None:
        async def go() -> None:
            session = _ScriptedSession([_FakeResponse(503), _FakeResponse(200, b"unreached")])
            resp = await session.post("http://127.0.0.1/", data=b"body")
            self.assertEqual(resp.status, 503)
            self.assertEqual(resp.keel_outcome["error"]["code"], "KEEL-E014")
            self.assertEqual(session.served, 1)

        self.run_async(go())

    def test_post_with_idempotency_key_is_retried(self) -> None:
        async def go() -> None:
            session = _ScriptedSession([_FakeResponse(503), _FakeResponse(200, b"posted")])
            resp = await session.post("http://127.0.0.1/", data=b"body", headers={"Idempotency-Key": "abc"})
            self.assertEqual(resp.status, 200)
            self.assertEqual(session.served, 2)

        self.run_async(go())

    def test_original_exception_reraised_unchanged(self) -> None:
        self.backend.configure({**level0_defaults(), "target": {"127.0.0.1": {"retry": {"attempts": 3, "on": ["timeout"], "schedule": "fixed(1ms)"}}}})

        async def go() -> None:
            session = _ScriptedSession([_FakeClientConnectionError("reset")])
            with self.assertRaises(_FakeClientConnectionError) as ctx:
                await session.get("http://127.0.0.1/")
            exc = ctx.exception
            self.assertNotIsInstance(exc, KeelError)
            self.assertEqual(exc.keel_outcome["error"]["code"], "KEEL-E015")
            self.assertIs(exc.keel_outcome["error"]["original"], exc)

        self.run_async(go())


class LlmHostMappingTest(AiohttpTestBase):
    def test_openai_host_resolves_to_llm_target(self) -> None:
        async def go() -> None:
            session = _ScriptedSession([_FakeResponse(200, b"{}")])
            await session.get("https://api.openai.com/v1/models")

        self.run_async(go())
        self.assertIn("llm:openai", self.rows())


class CacheReplayTest(AiohttpTestBase):
    def test_cache_hit_returns_a_replayed_response(self) -> None:
        self.backend.configure({**level0_defaults(), "target": {"127.0.0.1": {"cache": {"ttl": "10s"}}}})

        async def go() -> None:
            session = _ScriptedSession([_FakeResponse(200, b'{"a":1}', headers={"Content-Type": "application/json"})])
            first = await session.get("http://127.0.0.1/x")
            self.assertFalse(first.keel_outcome["from_cache"])
            second = await session.get("http://127.0.0.1/x")
            self.assertTrue(second.keel_outcome["from_cache"])
            self.assertNotIsInstance(second, _FakeResponse)  # a replayed facade, not the live type
            self.assertEqual(await second.read(), b'{"a":1}')
            self.assertEqual(await second.text(), '{"a":1}')
            self.assertEqual(await second.json(), {"a": 1})
            self.assertEqual(second.status, 200)
            self.assertTrue(second.ok)
            async with self.aiohttp_pack._ReplayedResponse(200, [], b"") as ctx_resp:
                self.assertIsInstance(ctx_resp, self.aiohttp_pack._ReplayedResponse)

        self.run_async(go())


class DisableTest(AiohttpTestBase):
    def test_keel_disable_is_transparent(self) -> None:
        _runtime.clear_runtime()

        async def go() -> None:
            session = _ScriptedSession([_FakeResponse(200, b"passthrough")])
            resp = await session.get("http://127.0.0.1/")
            self.assertEqual(await resp.read(), b"passthrough")
            self.assertFalse(hasattr(resp, "keel_outcome"))

        self.run_async(go())


if __name__ == "__main__":
    unittest.main()
