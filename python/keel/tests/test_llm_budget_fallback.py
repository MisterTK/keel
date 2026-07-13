"""Integration tests for LLM budget caps + model fallback chains at the
generic httpx/requests transport seams (Task L1). Mirrors
node/keel/test/fetch.test.mjs's budget/fallback section.

Uses the local FaultServer (no external network) with the target resolved via
``_http.LLM_HOST_PROVIDERS`` (monkeypatched to map the loopback host to
``llm:openai`` for the duration of each test, since the frozen provider-host
map only knows the real provider hostnames)."""

from __future__ import annotations

import asyncio
import json
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

import httpx
import requests

from keel import _runtime
from keel._backend import load_backend
from keel._errors import KeelError
from keel.adapters import _http, _llm_policy, httpx_pack, requests_pack

from .faultserver import FaultServer, fail, ok


def _openai_usage_body(reply: str = "hi") -> bytes:
    return json.dumps(
        {"reply": reply, "usage": {"prompt_tokens": 1_000_000, "completion_tokens": 1_000_000}}
    ).encode("utf-8")


class _LlmSeamBase(unittest.TestCase):
    """Shared fixture: registers the loopback host as `llm:openai` for the
    duration of the test (frozen `_http.LLM_HOST_PROVIDERS` only knows real
    provider hostnames), configures a stub backend, and installs a pack."""

    pack = None  # set by subclasses

    def setUp(self) -> None:
        _llm_policy.reset_llm_budgets()
        self._tmp = TemporaryDirectory()
        self.cwd = Path(self._tmp.name)
        self.backend = load_backend("stub")
        self._host_patch = dict(_http.LLM_HOST_PROVIDERS)
        _http.LLM_HOST_PROVIDERS = {**self._host_patch, "127.0.0.1": "openai"}  # type: ignore[misc]
        _runtime.set_runtime(self.backend, None)
        self.pack.install()

    def tearDown(self) -> None:
        self.pack.uninstall()
        _runtime.clear_runtime()
        _http.LLM_HOST_PROVIDERS = self._host_patch  # type: ignore[misc]
        self._tmp.cleanup()
        _llm_policy.reset_llm_budgets()

    def _post(self, url: str, body: dict) -> object:
        raise NotImplementedError


class HttpxBudgetFallbackTest(_LlmSeamBase):
    pack = httpx_pack

    def _post(self, url: str, body: dict) -> httpx.Response:
        with httpx.Client() as c:
            return c.post(url, content=json.dumps(body).encode("utf-8"), headers={"content-type": "application/json"})

    def test_usage_based_spend_blocks_the_next_call(self) -> None:
        self.backend.configure({"target": {"llm:openai": {"budget": "$1.00/run", "retry": {"attempts": 1}}}})
        with FaultServer([ok(_openai_usage_body()), ok(_openai_usage_body())]) as srv:
            r1 = self._post(srv.url(), {"model": "gpt-4o"})
            self.assertEqual(r1.status_code, 200)
            self.assertGreaterEqual(_llm_policy.spent_cents("llm:openai"), 100)

            with self.assertRaises(KeelError) as ctx:
                self._post(srv.url(), {"model": "gpt-4o"})
            self.assertEqual(ctx.exception.code, "KEEL-E012")
            self.assertIn("budget cap", str(ctx.exception).lower())
        self.assertEqual(srv.served, 1, "the second call was blocked before dispatch")

    def test_pre_exhausted_budget_blocks_the_first_call(self) -> None:
        _llm_policy.record_spend("llm:openai", 5)
        self.backend.configure({"target": {"llm:openai": {"budget": "$0.01/run"}}})
        with FaultServer([ok(b"{}")]) as srv:
            with self.assertRaises(KeelError) as ctx:
                self._post(srv.url(), {"model": "gpt-4o"})
            self.assertEqual(ctx.exception.code, "KEEL-E012")
        self.assertEqual(srv.served, 0)

    def test_no_budget_configured_never_reads_the_body_for_accounting(self) -> None:
        self.backend.configure({"target": {"llm:openai": {"retry": {"attempts": 1}}}})
        with FaultServer([ok(json.dumps({"usage": {"prompt_tokens": 999_999_999}}).encode("utf-8"))]) as srv:
            self._post(srv.url(), {"model": "gpt-4o"})
        self.assertEqual(_llm_policy.spent_cents("llm:openai"), 0)

    def test_fallback_re_dispatches_after_a_503_observed_not_retried(self) -> None:
        self.backend.configure(
            {"target": {"llm:openai": {"retry": {"attempts": 3}, "fallback": ["gpt-4o-mini"]}}}
        )
        with FaultServer([fail(503), ok(json.dumps({"reply": "fallback-ok"}).encode("utf-8"))]) as srv:
            resp = self._post(srv.url(), {"model": "gpt-4o", "messages": []})
        self.assertEqual(resp.status_code, 200)
        self.assertEqual(srv.served, 2)
        self.assertEqual(json.loads(srv.bodies[0])["model"], "gpt-4o")
        self.assertEqual(json.loads(srv.bodies[1])["model"], "gpt-4o-mini")

    def test_fallback_chain_exhausted_delivers_the_last_hop_failure(self) -> None:
        self.backend.configure(
            {"target": {"llm:openai": {"retry": {"attempts": 3}, "fallback": ["gpt-4o-mini", "gpt-4.1-mini"]}}}
        )
        with FaultServer([fail(503), fail(503), fail(503)]) as srv:
            resp = self._post(srv.url(), {"model": "gpt-4o"})
        self.assertEqual(resp.status_code, 503)
        self.assertEqual(srv.served, 3)
        self.assertEqual(resp.keel_outcome["error"]["code"], "KEEL-E014")

    def test_fallback_does_not_chase_a_budget_exceeded_block(self) -> None:
        _llm_policy.record_spend("llm:openai", 10)
        self.backend.configure({"target": {"llm:openai": {"budget": "$0.01/run", "fallback": ["gpt-4o-mini"]}}})
        with FaultServer([ok(b"{}")]) as srv:
            with self.assertRaises(KeelError) as ctx:
                self._post(srv.url(), {"model": "gpt-4o"})
            self.assertEqual(ctx.exception.code, "KEEL-E012")
        self.assertEqual(srv.served, 0)


class RequestsBudgetFallbackTest(_LlmSeamBase):
    pack = requests_pack

    def _post(self, url: str, body: dict) -> requests.Response:
        with requests.Session() as s:
            return s.post(url, data=json.dumps(body).encode("utf-8"), headers={"content-type": "application/json"})

    def test_usage_based_spend_blocks_the_next_call(self) -> None:
        self.backend.configure({"target": {"llm:openai": {"budget": "$1.00/run", "retry": {"attempts": 1}}}})
        with FaultServer([ok(_openai_usage_body()), ok(_openai_usage_body())]) as srv:
            r1 = self._post(srv.url(), {"model": "gpt-4o"})
            self.assertEqual(r1.status_code, 200)
            self.assertGreaterEqual(_llm_policy.spent_cents("llm:openai"), 100)

            with self.assertRaises(KeelError) as ctx:
                self._post(srv.url(), {"model": "gpt-4o"})
            self.assertEqual(ctx.exception.code, "KEEL-E012")
        self.assertEqual(srv.served, 1)

    def test_fallback_re_dispatches_after_a_503_observed_not_retried(self) -> None:
        self.backend.configure(
            {"target": {"llm:openai": {"retry": {"attempts": 3}, "fallback": ["gpt-4o-mini"]}}}
        )
        with FaultServer([fail(503), ok(json.dumps({"reply": "fallback-ok"}).encode("utf-8"))]) as srv:
            resp = self._post(srv.url(), {"model": "gpt-4o", "messages": []})
        self.assertEqual(resp.status_code, 200)
        self.assertEqual(srv.served, 2)
        self.assertEqual(json.loads(srv.bodies[0])["model"], "gpt-4o")
        self.assertEqual(json.loads(srv.bodies[1])["model"], "gpt-4o-mini")

    def test_fallback_unrecognized_shape_stops_the_chain(self) -> None:
        self.backend.configure({"target": {"llm:openai": {"fallback": ["gpt-4o-mini"]}}})
        with FaultServer([fail(503)]) as srv:
            with requests.Session() as s:
                resp = s.post(srv.url(), data=b"not json", headers={"content-type": "text/plain"})
        self.assertEqual(resp.status_code, 503)
        self.assertEqual(srv.served, 1, "no `model` field to rewrite; fallback never re-dispatched")


class AsyncHttpxBudgetFallbackTest(_LlmSeamBase):
    pack = httpx_pack

    def test_async_fallback_re_dispatches_after_a_503(self) -> None:
        self.backend.configure(
            {"target": {"llm:openai": {"retry": {"attempts": 3}, "fallback": ["gpt-4o-mini"]}}}
        )
        with FaultServer([fail(503), ok(json.dumps({"reply": "async-fallback-ok"}).encode("utf-8"))]) as srv:

            async def go() -> httpx.Response:
                async with httpx.AsyncClient() as c:
                    return await c.post(
                        srv.url(),
                        content=json.dumps({"model": "gpt-4o"}).encode("utf-8"),
                        headers={"content-type": "application/json"},
                    )

            resp = asyncio.run(go())
        self.assertEqual(resp.status_code, 200)
        self.assertEqual(srv.served, 2)
        self.assertEqual(json.loads(srv.bodies[1])["model"], "gpt-4o-mini")


if __name__ == "__main__":
    unittest.main()
