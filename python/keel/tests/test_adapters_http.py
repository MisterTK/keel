"""The shared HTTP judgments (`keel.adapters._http`), tested directly.

These derivations are a cross-language parity contract with the Node twin
(`node/keel/src/judge.mjs`); the assertions below pin the exact rules —
host→llm mapping, the idempotent-method set, POST-with-key, args_hash gating,
Retry-After parsing (both RFC forms), and the transient-status boundary."""

from __future__ import annotations

import unittest
from datetime import datetime, timedelta, timezone

import httpx
from requests.models import PreparedRequest

from keel import _targets
from keel.adapters import _http, httpx_pack, requests_pack


class ResolveTargetTest(unittest.TestCase):
    def test_llm_hosts_map_to_providers(self) -> None:
        self.assertEqual(_http.resolve_target("api.openai.com"), "llm:openai")
        self.assertEqual(_http.resolve_target("api.anthropic.com"), "llm:anthropic")
        self.assertEqual(
            _http.resolve_target("generativelanguage.googleapis.com"), "llm:google-genai"
        )

    def test_unknown_host_is_its_own_target(self) -> None:
        self.assertEqual(_http.resolve_target("example.com"), "example.com")
        self.assertEqual(_http.resolve_target("127.0.0.1"), "127.0.0.1")


class ResolvePolicyTargetTest(unittest.TestCase):
    """`resolve_policy_target` (docs/targeting.md): the LLM host map first,
    then `_targets`'s exact/pattern/default resolution, driven by whatever
    outbound matchers are process-installed (`_targets.install_outbound_targets`,
    as `bootstrap.install_keel` does from the effective policy)."""

    def tearDown(self) -> None:
        _targets.clear_outbound_targets()

    def test_no_matchers_installed_matches_resolve_target(self) -> None:
        self.assertEqual(
            _http.resolve_policy_target("GET", "api.stripe.com"), "api.stripe.com"
        )
        self.assertEqual(
            _http.resolve_policy_target("POST", "api.openai.com"), "llm:openai"
        )

    def test_llm_host_map_wins_even_over_an_installed_pattern(self) -> None:
        _targets.install_outbound_targets({"target": {"*.openai.com": {}}})
        self.assertEqual(
            _http.resolve_policy_target("POST", "api.openai.com"), "llm:openai"
        )

    def test_exact_host_key_beats_an_installed_pattern(self) -> None:
        _targets.install_outbound_targets(
            {"target": {"api.internal": {}, "*.internal": {}}}
        )
        self.assertEqual(_http.resolve_policy_target("GET", "api.internal"), "api.internal")

    def test_pattern_key_selected_by_method_port_and_path(self) -> None:
        _targets.install_outbound_targets(
            {"target": {"GET api.catalog.internal/*": {}}}
        )
        self.assertEqual(
            _http.resolve_policy_target(
                "GET", "api.catalog.internal", scheme="https", path="/items/5"
            ),
            "GET api.catalog.internal/*",
        )
        self.assertEqual(
            _http.resolve_policy_target(
                "POST", "api.catalog.internal", scheme="https", path="/items/5"
            ),
            "api.catalog.internal",
            "a POST does not match a GET-prefixed pattern -> falls through",
        )

    def test_wildcard_host_segment_matches_subdomains(self) -> None:
        _targets.install_outbound_targets({"target": {"*.internal.corp": {}}})
        self.assertEqual(
            _http.resolve_policy_target("GET", "db.internal.corp"), "*.internal.corp"
        )
        self.assertEqual(
            _http.resolve_policy_target("GET", "unrelated.example.com"),
            "unrelated.example.com",
        )


class IdempotencyTest(unittest.TestCase):
    def test_safe_and_rfc_idempotent_methods_are_idempotent(self) -> None:
        for m in ("GET", "HEAD", "OPTIONS", "TRACE", "PUT", "DELETE"):
            self.assertTrue(_http.is_idempotent(m, []), m)

    def test_post_and_patch_are_not_idempotent_without_a_key(self) -> None:
        self.assertFalse(_http.is_idempotent("POST", ["content-type"]))
        self.assertFalse(_http.is_idempotent("PATCH", []))

    def test_post_with_idempotency_key_header_is_idempotent(self) -> None:
        self.assertTrue(_http.is_idempotent("POST", ["Idempotency-Key"]))
        self.assertTrue(_http.is_idempotent("POST", ["X-Idempotency-Key"]))

    def test_configured_header_overrides_default_set(self) -> None:
        self.assertTrue(_http.is_idempotent("POST", ["My-Key"], idempotency_header="My-Key"))
        # With a configured header, the defaults no longer apply.
        self.assertFalse(_http.is_idempotent("POST", ["Idempotency-Key"], idempotency_header="My-Key"))


class ArgsHashTest(unittest.TestCase):
    def test_stable_and_url_sensitive(self) -> None:
        a = _http.args_hash("GET", "https://h/x")
        self.assertEqual(a, _http.args_hash("GET", "https://h/x"))
        self.assertNotEqual(a, _http.args_hash("GET", "https://h/y"))
        self.assertEqual(len(a), 64)  # sha256 hex

    def test_body_participates(self) -> None:
        self.assertNotEqual(
            _http.args_hash("GET", "https://h/x", b"a"),
            _http.args_hash("GET", "https://h/x", b"b"),
        )


class RetryAfterTest(unittest.TestCase):
    def test_delta_seconds(self) -> None:
        self.assertEqual(_http.parse_retry_after("2"), 2000)
        self.assertEqual(_http.parse_retry_after("0"), 0)

    def test_http_date(self) -> None:
        now = datetime(2026, 7, 12, 12, 0, 0, tzinfo=timezone.utc)
        future = now + timedelta(seconds=5)
        header = future.strftime("%a, %d %b %Y %H:%M:%S GMT")
        self.assertEqual(_http.parse_retry_after(header, now=now), 5000)

    def test_past_date_clamps_to_zero(self) -> None:
        now = datetime(2026, 7, 12, 12, 0, 0, tzinfo=timezone.utc)
        past = (now - timedelta(seconds=30)).strftime("%a, %d %b %Y %H:%M:%S GMT")
        self.assertEqual(_http.parse_retry_after(past, now=now), 0)

    def test_iso8601_date_is_honored(self) -> None:
        # A server that emits an ISO-8601 Retry-After (a common extension) must be
        # honored, matching Node's Date.parse (parity — Node already accepts it).
        now = datetime(2026, 7, 12, 12, 0, 0, tzinfo=timezone.utc)
        self.assertEqual(_http.parse_retry_after("2026-07-12T12:00:03Z", now=now), 3000)
        self.assertEqual(_http.parse_retry_after("2026-07-12T12:00:03+00:00", now=now), 3000)

    def test_unparseable_and_none(self) -> None:
        self.assertIsNone(_http.parse_retry_after(None))
        self.assertIsNone(_http.parse_retry_after("soon"))

    def test_non_ascii_digits_are_not_seconds(self) -> None:
        # Node's /^\d+$/ matches ASCII digits only; exotic digits must not be
        # taken as delta-seconds (nor raise on int()).
        self.assertIsNone(_http.parse_retry_after("１２３"))  # fullwidth digits


class CrossJudgeParityTest(unittest.TestCase):
    """A no-body GET must produce the SAME args_hash from the httpx and requests
    judges (and match method+url with no body) — cross-adapter cache-key parity."""

    def test_no_body_get_hashes_identically_across_judges(self) -> None:
        url = "https://example.com/p"
        _, _, _, hx_hash = httpx_pack._judge(httpx.Request("GET", url))
        prepared = PreparedRequest()
        prepared.prepare(method="GET", url=url)
        _, _, _, rq_hash = requests_pack._judge(prepared)
        self.assertIsNotNone(hx_hash)
        self.assertEqual(hx_hash, rq_hash)
        self.assertEqual(hx_hash, _http.args_hash("GET", url))  # no trailing body separator


class TransientStatusTest(unittest.TestCase):
    def test_429_and_5xx_are_transient(self) -> None:
        for s in (429, 500, 502, 503, 504, 599):
            self.assertTrue(_http.is_transient_status(s), s)

    def test_2xx_3xx_and_non_429_4xx_are_not_transient(self) -> None:
        for s in (200, 201, 301, 400, 404, 409, 418, 428):
            self.assertFalse(_http.is_transient_status(s), s)


if __name__ == "__main__":
    unittest.main()
