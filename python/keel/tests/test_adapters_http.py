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

from keel import _runtime
from keel.adapters import _http, httpx_pack, requests_pack
from keel_core_stub import KeelCoreStub

try:
    import keel_core  # noqa: F401

    _NATIVE = True
except ImportError:
    _NATIVE = False


class ResolveTargetTest(unittest.TestCase):
    """`backend.resolve_target` (docs/targeting.md): the LLM host map, tested
    directly against the pure-Python stub (Task 7/SP-1's mirror of the native
    core's matcher; conformance scenarios 36-38 prove native == stub == Node
    on every precedence dimension, so this offline suite exercises the stub
    as a faithful, standalone oracle)."""

    def test_llm_hosts_map_to_providers(self) -> None:
        backend = KeelCoreStub()
        self.assertEqual(backend.resolve_target("GET", "api.openai.com"), "llm:openai")
        self.assertEqual(backend.resolve_target("GET", "api.anthropic.com"), "llm:anthropic")
        self.assertEqual(
            backend.resolve_target("GET", "generativelanguage.googleapis.com"),
            "llm:google-genai",
        )

    def test_unknown_host_is_its_own_target(self) -> None:
        backend = KeelCoreStub()
        self.assertEqual(backend.resolve_target("GET", "example.com"), "example.com")
        self.assertEqual(backend.resolve_target("GET", "127.0.0.1"), "127.0.0.1")


class KnownLlmHostsTest(unittest.TestCase):
    """`_http.known_llm_hosts()` (issue #49): the enumeration twin of
    `resolve_target`'s single-lookup form, delegating to the always-available
    pure-Python stub rather than `_runtime.get_backend()` (no live runtime is
    required to enumerate — see the function's own docstring)."""

    def test_delegates_to_the_stubs_enumeration(self) -> None:
        self.assertEqual(_http.known_llm_hosts(), KeelCoreStub.known_llm_hosts())

    def test_every_llm_host_resolve_target_maps_appears_in_the_enumeration(self) -> None:
        # The enumeration and the single-lookup form must agree: every pair
        # `known_llm_hosts()` lists must resolve to `llm:<provider>` via
        # `resolve_target`, and vice versa (no drift between the two forms).
        backend = KeelCoreStub()
        hosts = _http.known_llm_hosts()
        host_set = {h for h, _ in hosts}
        for host in ("api.openai.com", "api.anthropic.com", "generativelanguage.googleapis.com"):
            self.assertIn(host, host_set)
        for host, provider in hosts:
            self.assertEqual(backend.resolve_target("GET", host), f"llm:{provider}")

    def test_no_active_runtime_required(self) -> None:
        # targets() (keel doctor/keel init) may enumerate before any backend
        # is installed — this must not raise, regardless of what another
        # test in this process happened to install.
        _runtime.clear_runtime()
        self.assertIsNone(_runtime.get_backend())
        self.assertTrue(len(_http.known_llm_hosts()) > 0)

    @unittest.skipUnless(_NATIVE, "keel_core native module not built (maturin develop in crates/keel-py)")
    def test_matches_the_native_core_when_built(self) -> None:
        # The stub's table is one of THREE copies kept in parity by
        # convention (Rust authoritative + Python stub + Node stub) — this
        # cross-checks the stub against the real Rust source of truth
        # whenever the native wheel is available (offline runs skip; a
        # native-leg CI run does not), closing the gap the review of issue
        # #49 identified: nothing previously compared the tables to each
        # other, only each to its own resolve_target.
        self.assertEqual(
            sorted(_http.known_llm_hosts()),
            sorted(keel_core.KeelCore.known_llm_hosts()),
        )


class ResolvePolicyTargetTest(unittest.TestCase):
    """`backend.resolve_target` (docs/targeting.md): the LLM host map first,
    then the configured `[target]` table's exact/pattern/default resolution —
    driven by whatever policy the backend was `configure`d with (no separate
    install step; the backend re-derives its matchers from the configured
    policy on every call, per Task 10/SP-1)."""

    @staticmethod
    def _backend(policy: dict | None = None) -> KeelCoreStub:
        backend = KeelCoreStub()
        backend.configure(policy or {})
        return backend

    def test_no_matchers_installed_matches_resolve_target(self) -> None:
        backend = self._backend()
        self.assertEqual(backend.resolve_target("GET", "api.stripe.com"), "api.stripe.com")
        self.assertEqual(backend.resolve_target("POST", "api.openai.com"), "llm:openai")

    def test_llm_host_map_wins_even_over_an_installed_pattern(self) -> None:
        backend = self._backend({"target": {"*.openai.com": {}}})
        self.assertEqual(backend.resolve_target("POST", "api.openai.com"), "llm:openai")

    def test_exact_host_key_beats_an_installed_pattern(self) -> None:
        backend = self._backend({"target": {"api.internal": {}, "*.internal": {}}})
        self.assertEqual(backend.resolve_target("GET", "api.internal"), "api.internal")

    def test_pattern_key_selected_by_method_port_and_path(self) -> None:
        backend = self._backend({"target": {"GET api.catalog.internal/*": {}}})
        self.assertEqual(
            backend.resolve_target(
                "GET", "api.catalog.internal", scheme="https", path="/items/5"
            ),
            "GET api.catalog.internal/*",
        )
        self.assertEqual(
            backend.resolve_target(
                "POST", "api.catalog.internal", scheme="https", path="/items/5"
            ),
            "api.catalog.internal",
            "a POST does not match a GET-prefixed pattern -> falls through",
        )

    def test_wildcard_host_segment_matches_subdomains(self) -> None:
        backend = self._backend({"target": {"*.internal.corp": {}}})
        self.assertEqual(backend.resolve_target("GET", "db.internal.corp"), "*.internal.corp")
        self.assertEqual(
            backend.resolve_target("GET", "unrelated.example.com"),
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


class IdempotencyInjectionTest(unittest.TestCase):
    """contracts/adapter-pack.md "Idempotency-key injection" — the mint+inject
    decision, pinned independently of any HTTP library."""

    def test_no_configured_header_means_no_injection(self) -> None:
        self.assertIsNone(_http.resolve_idempotency_injection("POST", [], None))

    def test_idempotent_methods_are_never_injected(self) -> None:
        for m in ("GET", "HEAD", "OPTIONS", "PUT", "DELETE", "TRACE"):
            self.assertIsNone(
                _http.resolve_idempotency_injection(m, [], "Idempotency-Key"), m
            )

    def test_caller_supplied_key_always_wins_never_overwritten(self) -> None:
        self.assertIsNone(
            _http.resolve_idempotency_injection(
                "POST", ["idempotency-key"], "Idempotency-Key"
            )
        )
        # Case-insensitive match.
        self.assertIsNone(
            _http.resolve_idempotency_injection(
                "POST", ["IDEMPOTENCY-KEY"], "Idempotency-Key"
            )
        )

    def test_unsafe_method_with_configured_header_mints_a_key(self) -> None:
        key = _http.resolve_idempotency_injection("POST", [], "Idempotency-Key")
        self.assertIsInstance(key, str)
        self.assertTrue(key)

    def test_mint_is_deterministic_under_test(self) -> None:
        # rule 2 requires a fresh mint per logical call; the source is
        # injectable (a plain module attribute) so tests never depend on real
        # randomness.
        orig = _http.new_idempotency_key
        try:
            calls = iter(["fixed-key-1", "fixed-key-2"])
            _http.new_idempotency_key = lambda: next(calls)  # type: ignore[assignment]
            self.assertEqual(
                _http.resolve_idempotency_injection("POST", [], "Idempotency-Key"),
                "fixed-key-1",
            )
            self.assertEqual(
                _http.resolve_idempotency_injection("PATCH", [], "Idempotency-Key"),
                "fixed-key-2",
            )
        finally:
            _http.new_idempotency_key = orig

    def test_a_tier2_recorded_key_is_reused_verbatim_rule_3(self) -> None:
        # A resume that re-executes a crashed step injects the SAME key the
        # journal recorded — never a fresh mint — so the provider can dedup.
        self.assertEqual(
            _http.resolve_idempotency_injection(
                "POST", [], "Idempotency-Key", recorded_key="ik-recorded"
            ),
            "ik-recorded",
        )

    def test_new_idempotency_key_mints_distinct_opaque_values(self) -> None:
        a, b = _http.new_idempotency_key(), _http.new_idempotency_key()
        self.assertNotEqual(a, b)
        self.assertTrue(a)


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
    judges (and match method+url with no body) — cross-adapter cache-key parity.

    `_judge` resolves its target via `_runtime.get_backend().resolve_target`
    (Task 10/SP-1), so calling it directly needs an active backend; a bare,
    unconfigured `KeelCoreStub` is sufficient (this test only checks args_hash
    parity, not target resolution itself — that's `ResolveTargetTest`/
    `ResolvePolicyTargetTest` above)."""

    def setUp(self) -> None:
        _runtime.set_runtime(KeelCoreStub(), None)

    def tearDown(self) -> None:
        _runtime.clear_runtime()

    def test_no_body_get_hashes_identically_across_judges(self) -> None:
        url = "https://example.com/p"
        _, _, _, hx_hash, _ = httpx_pack._judge(httpx.Request("GET", url))
        prepared = PreparedRequest()
        prepared.prepare(method="GET", url=url)
        _, _, _, rq_hash, _ = requests_pack._judge(prepared)
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
