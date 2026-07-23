"""End-to-end LLM pack tests through the real httpx/requests transports against
fake OpenAI/Anthropic-shaped endpoints on the local fault server, driven by the
full front end (`install_keel` → policy merge → dev-cache resolution → backend).

Covers the brief's item-3 acceptance:
  * a 429-storm with Retry-After survives per the llm defaults (a retryable llm
    call rides it out; Retry-After governs the backoff);
  * an identical prompt twice under KEEL_ENV=dev hits the cache (second call
    attempts=0, from_cache, API call count 1);
  * KEEL_ENV=prod bypasses the cache;
  * per-provider target resolution lands in the report under llm:openai /
    llm:anthropic.

The local fault server binds 127.0.0.1, so each test patches the INSTALLED
BACKEND's `resolve_target` to route the loopback host to the provider under
test — the same seam the real api.openai.com / api.anthropic.com hosts use.
Since Task 10/SP-1 moved target resolution into the backend (native core or
stub), the map lives there now, not in a front-end dict a test could
monkeypatch process-wide — so `map_host` patches the specific backend
instance `install()` returns, per test.
"""

from __future__ import annotations

import unittest
from tempfile import TemporaryDirectory
from typing import Any
from unittest import mock

import httpx
import requests

from keel.bootstrap import install_keel, uninstall_keel

from .faultserver import FaultServer, ok, throttled

_JSON = {"Content-Type": "application/json"}


def _chat_body(prompt: str) -> dict[str, Any]:
    return {"model": "gpt-x", "messages": [{"role": "user", "content": prompt}]}


class LlmE2EBase(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = TemporaryDirectory()
        self.cwd = self._tmp.name

    def tearDown(self) -> None:
        uninstall_keel()
        self._tmp.cleanup()

    def install(self, **env: str) -> dict[str, Any]:
        """Install the front end with a Level 0 policy (no keel.toml in cwd), the
        stub backend, and a silenced banner."""
        base = {"KEEL_BACKEND": "stub", "KEEL_QUIET": "1"}
        base.update(env)
        return install_keel(cwd=self.cwd, env=base)

    @staticmethod
    def map_host(backend: Any, host: str, provider: str) -> Any:
        """Map `host` to `llm:<provider>` on THIS `backend` instance only, for
        the duration of the returned context manager (restores the instance's
        original `resolve_target` on exit)."""
        orig = backend.resolve_target

        def mapped(method: str, h: str, *, scheme: str | None = None, port: int | None = None, path: str | None = None) -> str:
            if h == host:
                return f"llm:{provider}"
            return orig(method, h, scheme=scheme, port=port, path=path)

        return mock.patch.object(backend, "resolve_target", mapped)


class DevCacheReplayTest(LlmE2EBase):
    def test_identical_prompt_replays_from_cache_off_prod(self) -> None:
        with FaultServer([ok(b'{"choices":[{"message":{"content":"hi"}}]}', _JSON)]) as srv:
            backend = self.install()["backend"]  # KEEL_ENV unset → dev
            with self.map_host(backend, "127.0.0.1", "openai"):
                with httpx.Client() as c:
                    r1 = c.post(srv.url("/v1/chat/completions"), json=_chat_body("hello"))
                    first_outcome = r1.keel_outcome  # capture before the replay rebinds it
                    r2 = c.post(srv.url("/v1/chat/completions"), json=_chat_body("hello"))
                self.assertEqual(r1.status_code, 200)
                self.assertEqual(r2.status_code, 200)
                self.assertEqual(srv.served, 1, "second identical prompt served from cache")
                self.assertFalse(first_outcome["from_cache"])
                self.assertTrue(r2.keel_outcome["from_cache"])
                self.assertEqual(r2.keel_outcome["attempts"], 0)
                self.assertEqual(r2.content, r1.content)
                self.assertEqual(
                    backend.report()["targets"]["llm:openai"],
                    {
                        "attempts": 1,
                        "breaker_opens": 0,
                        "breaker_state": "closed",
                        "cache_hits": 1,
                        "calls": 2,
                        "failures": 0,
                        "retries": 0,
                        "successes": 2,
                        "throttled": 0,
                    },
                )

    def test_requests_adapter_replays_too(self) -> None:
        with FaultServer([ok(b'{"choices":[]}', _JSON)]) as srv:
            backend = self.install()["backend"]
            with self.map_host(backend, "127.0.0.1", "openai"):
                session = requests.Session()
                session.post(srv.url("/v1/chat/completions"), json=_chat_body("hi"))
                r2 = session.post(srv.url("/v1/chat/completions"), json=_chat_body("hi"))
                self.assertEqual(srv.served, 1)
                self.assertTrue(r2.keel_outcome["from_cache"])
                self.assertEqual(backend.report()["targets"]["llm:openai"]["cache_hits"], 1)


class DevCacheProdBypassTest(LlmE2EBase):
    def test_prod_bypasses_dev_cache(self) -> None:
        with FaultServer([ok(b'{"a":1}', _JSON), ok(b'{"a":2}', _JSON)]) as srv:
            backend = self.install(KEEL_ENV="prod")["backend"]
            with self.map_host(backend, "127.0.0.1", "openai"):
                with httpx.Client() as c:
                    c.post(srv.url("/v1/chat/completions"), json=_chat_body("hello"))
                    r2 = c.post(srv.url("/v1/chat/completions"), json=_chat_body("hello"))
                self.assertFalse(r2.keel_outcome["from_cache"], "no dev cache in prod")
                self.assertEqual(srv.served, 2)
                self.assertEqual(
                    backend.report()["targets"]["llm:openai"],
                    {
                        "attempts": 2,
                        "breaker_opens": 0,
                        "breaker_state": "closed",
                        "cache_hits": 0,
                        "calls": 2,
                        "failures": 0,
                        "retries": 0,
                        "successes": 2,
                        "throttled": 0,
                    },
                )


class LlmRetryStormTest(LlmE2EBase):
    def test_429_storm_with_retry_after_survives_per_llm_defaults(self) -> None:
        # A retryable llm call (POST + Idempotency-Key) rides out a 429 storm per
        # the llm defaults (attempts=6). Retry-After (1s) governs each backoff
        # over the llm schedule (500ms → 1000ms). KEEL_ENV=prod isolates retry
        # from the dev cache. Non-idempotent LLM POSTs are NOT retried (Level 0
        # hard rule) — that is covered by the httpx adapter suite.
        script = [throttled("1"), throttled("1"), ok(b'{"choices":[]}', _JSON)]
        with FaultServer(script) as srv:
            backend = self.install(KEEL_ENV="prod")["backend"]
            with self.map_host(backend, "127.0.0.1", "openai"):
                with httpx.Client() as c:
                    r = c.post(
                        srv.url("/v1/chat/completions"),
                        json=_chat_body("hi"),
                        headers={"Idempotency-Key": "storm-1"},
                    )
                self.assertEqual(r.status_code, 200)
                self.assertEqual(r.keel_outcome["attempts"], 3)
                self.assertEqual(srv.served, 3)
                # max(schedule_wait, retry_after) each round: max(500,1000), max(1000,1000).
                self.assertEqual(r.keel_outcome["waits_ms"], [1000, 1000])
                t = backend.report()["targets"]["llm:openai"]
                self.assertEqual(t["successes"], 1)
                self.assertEqual(t["retries"], 2)
                self.assertEqual(t["failures"], 0)


class PerProviderReportTest(LlmE2EBase):
    def test_both_providers_land_under_their_targets(self) -> None:
        with FaultServer([ok(b'{"o":1}', _JSON)]) as srv_o, FaultServer(
            [ok(b'{"a":1}', _JSON)]
        ) as srv_a:
            backend = self.install(KEEL_ENV="prod")["backend"]
            with self.map_host(backend, "127.0.0.1", "openai"):
                with httpx.Client() as c:
                    c.post(srv_o.url("/v1/chat/completions"), json=_chat_body("o"))
            with self.map_host(backend, "127.0.0.1", "anthropic"):
                with httpx.Client() as c:
                    c.post(srv_a.url("/v1/messages"), json=_chat_body("a"))
        targets = backend.report()["targets"]
        self.assertIn("llm:openai", targets)
        self.assertIn("llm:anthropic", targets)
        self.assertEqual(targets["llm:openai"]["calls"], 1)
        self.assertEqual(targets["llm:openai"]["successes"], 1)
        self.assertEqual(targets["llm:anthropic"]["calls"], 1)
        self.assertEqual(targets["llm:anthropic"]["successes"], 1)


if __name__ == "__main__":
    unittest.main()
