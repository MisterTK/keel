"""LLM pack unit + parity tests: the adapter-pack four operations, the
defaults-merge semantics, dev-cache resolution, and the dev-cache replay
counters — the cross-language parity contract with the Node twin
(node/keel/test/llm-pack.test.mjs: same policy in → same report counters out).

Also verifies the item-2 requirement directly: the core cache path honors
``args_hash`` on NON-idempotent requests (dev-cache replay of an LLM POST), so
the args_hash exception needs no change to shared stub/core semantics.
"""

from __future__ import annotations

import importlib.util
import unittest
from typing import Any, Callable

import httpx
from requests.models import PreparedRequest

from keel import _runtime
from keel._defaults import (
    apply_pack_defaults,
    level0_defaults,
    llm_defaults,
    outbound_defaults,
)
from keel.adapters import _http, httpx_pack, requests_pack
from keel.packs import (
    DEV_CACHE_TTL,
    anthropic_pack,
    google_genai_pack,
    llm_pack,
    openai_pack,
    present_provider_defaults,
    resolve_dev_cache,
)
from keel.packs._provider import module_present
from keel_core_stub import KeelCoreStub


def _req(target: str, args_hash: str, *, idempotent: bool = False) -> dict[str, Any]:
    # idempotent defaults to False on purpose: an LLM POST stays non-idempotent
    # (Level 0 hard rule); the dev cache still replays it (item 2).
    return {"v": 1, "target": target, "op": target, "idempotent": idempotent, "args_hash": args_hash}


def _ok(payload: Any) -> Callable[[int], dict[str, Any]]:
    return lambda _attempt: {"status": "ok", "payload": payload}


class LlmPackContractTest(unittest.TestCase):
    def test_four_operations(self) -> None:
        det = llm_pack.detect()
        self.assertTrue(det.matched)
        self.assertEqual(det.name, "llm")
        self.assertEqual(det.confidence, "pinned")
        self.assertEqual(llm_pack.seams(), [])  # targets come from other seams
        targets = llm_pack.targets()
        self.assertEqual(len(targets), 1)
        self.assertEqual(targets[0].pattern, "llm:<provider>")
        self.assertEqual(targets[0].kind, "llm")
        self.assertEqual(llm_pack.defaults(), {"defaults": {"llm": llm_defaults()}})


class ApplyPackDefaultsTest(unittest.TestCase):
    """Mirror of node/keel/test/llm-pack.test.mjs 'applyPackDefaults …'."""

    def test_empty_policy_gets_full_pack_layers(self) -> None:
        empty = apply_pack_defaults({})
        self.assertEqual(empty["defaults"]["outbound"], outbound_defaults())
        self.assertEqual(empty["defaults"]["llm"], llm_defaults())

    def test_user_key_replaces_pack_key_wholesale(self) -> None:
        u = apply_pack_defaults(
            {"defaults": {"llm": {"retry": {"attempts": 2}}}, "target": {"x": {}}}
        )
        self.assertEqual(u["defaults"]["llm"]["retry"], {"attempts": 2}, "user retry wins wholesale")
        self.assertEqual(u["defaults"]["llm"]["cache"], {"mode": "dev"}, "pack cache kept")
        self.assertEqual(
            u["defaults"]["llm"]["breaker"], {"failures": 5, "cooldown": "30s"}, "pack breaker kept"
        )
        self.assertEqual(u["target"], {"x": {}}, "target tables untouched")

    def test_idempotent_on_level0(self) -> None:
        self.assertEqual(apply_pack_defaults(level0_defaults()), level0_defaults())

    def test_never_mutates_input(self) -> None:
        orig = {"defaults": {"llm": {"retry": {"attempts": 9}}}}
        apply_pack_defaults(orig)
        self.assertEqual(orig, {"defaults": {"llm": {"retry": {"attempts": 9}}}})

    def test_provider_fragments_fold_as_identity(self) -> None:
        # defaults < packs < user: the provider packs emit the generic llm layer,
        # so folding their fragments changes nothing over the embedded defaults.
        fragments = [openai_pack.defaults(), anthropic_pack.defaults(), google_genai_pack.defaults()]
        self.assertEqual(apply_pack_defaults({}, fragments), apply_pack_defaults({}))


class ResolveDevCacheTest(unittest.TestCase):
    """Mirror of node/keel/test/llm-pack.test.mjs 'resolveDevCache …'."""

    @staticmethod
    def _raw() -> dict[str, Any]:
        return {"target": {"llm:openai": {"cache": {"mode": "dev"}}}}

    def test_mode_dev_becomes_ttl_off_prod(self) -> None:
        got = resolve_dev_cache(self._raw(), {})
        self.assertEqual(got["target"]["llm:openai"]["cache"], {"ttl": DEV_CACHE_TTL})

    def test_removed_in_prod(self) -> None:
        got = resolve_dev_cache(self._raw(), {"KEEL_ENV": "prod"})
        self.assertNotIn("cache", got["target"]["llm:openai"], "dev cache is inert in prod")

    def test_explicit_ttl_preserved(self) -> None:
        with_ttl = {"defaults": {"llm": {"cache": {"mode": "dev", "ttl": "5m"}}}}
        got = resolve_dev_cache(with_ttl, {})
        self.assertEqual(got["defaults"]["llm"]["cache"], {"ttl": "5m"})

    def test_non_dev_and_inputs_untouched(self) -> None:
        plain = {"target": {"svc": {"cache": {"ttl": "10s"}}}}
        self.assertEqual(resolve_dev_cache(plain, {"KEEL_ENV": "prod"}), plain)

    def test_does_not_mutate_input(self) -> None:
        raw = self._raw()
        resolve_dev_cache(raw, {})
        self.assertEqual(raw["target"]["llm:openai"]["cache"], {"mode": "dev"})


class DevCacheReplayCountersTest(unittest.TestCase):
    """The report-counter parity contract with the Node twin, driven through the
    stub core directly (as Node drives its AsyncEngine)."""

    def test_identical_calls_hit_cache_off_prod(self) -> None:
        policy = resolve_dev_cache(
            {"target": {"llm:openai": {"cache": {"mode": "dev"}, "retry": {"attempts": 1}}}}, {}
        )
        core = KeelCoreStub()
        core.configure(policy)
        n = {"calls": 0}

        def effect(payload: Any) -> Callable[[int], dict[str, Any]]:
            def run(_attempt: int) -> dict[str, Any]:
                n["calls"] += 1
                return {"status": "ok", "payload": payload}

            return run

        o1 = core.execute(_req("llm:openai", "h"), effect({"text": "hi"}))
        o2 = core.execute(_req("llm:openai", "h"), effect({"text": "SHOULD-NOT-RUN"}))

        self.assertEqual(n["calls"], 1, "second identical call is served from cache")
        self.assertFalse(o1["from_cache"])
        self.assertTrue(o2["from_cache"])
        self.assertEqual(o2["attempts"], 0)
        self.assertEqual(o2["payload"], {"text": "hi"})
        self.assertEqual(
            core.report()["targets"]["llm:openai"],
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

    def test_inert_under_prod(self) -> None:
        policy = resolve_dev_cache(
            {"target": {"llm:openai": {"cache": {"mode": "dev"}, "retry": {"attempts": 1}}}},
            {"KEEL_ENV": "prod"},
        )
        core = KeelCoreStub()
        core.configure(policy)
        core.execute(_req("llm:openai", "h"), _ok({"text": "a"}))
        o2 = core.execute(_req("llm:openai", "h"), _ok({"text": "b"}))
        self.assertFalse(o2["from_cache"], "no dev cache in prod")
        self.assertEqual(
            core.report()["targets"]["llm:openai"],
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

    def test_full_level0_path(self) -> None:
        policy = resolve_dev_cache(apply_pack_defaults({}), {})
        self.assertEqual(policy["defaults"]["llm"]["cache"], {"ttl": DEV_CACHE_TTL})
        core = KeelCoreStub()
        core.configure(policy)  # must validate (delegates to the stub validator)
        core.execute(_req("llm:openai", "h"), _ok({"text": "hi"}))
        o2 = core.execute(_req("llm:openai", "h"), _ok({"text": "no"}))
        self.assertTrue(o2["from_cache"])
        t = core.report()["targets"]["llm:openai"]
        self.assertEqual(t["cache_hits"], 1)
        self.assertEqual(t["calls"], 2)
        self.assertEqual(t["attempts"], 1)


class ProviderPackTest(unittest.TestCase):
    def test_openai_pack(self) -> None:
        present = importlib.util.find_spec("openai") is not None
        self.assertEqual(openai_pack.detect().matched, present)
        self.assertEqual(openai_pack.seams(), [])
        targets = openai_pack.targets()
        self.assertEqual(targets[0].pattern, "llm:openai")
        self.assertEqual(targets[0].kind, "llm")
        self.assertEqual(openai_pack.defaults(), {"defaults": {"llm": llm_defaults()}})

    def test_anthropic_pack(self) -> None:
        present = importlib.util.find_spec("anthropic") is not None
        self.assertEqual(anthropic_pack.detect().matched, present)
        self.assertEqual(anthropic_pack.seams(), [])
        targets = anthropic_pack.targets()
        self.assertEqual(targets[0].pattern, "llm:anthropic")
        self.assertEqual(targets[0].kind, "llm")
        self.assertEqual(anthropic_pack.defaults(), {"defaults": {"llm": llm_defaults()}})

    def test_google_genai_pack(self) -> None:
        present = module_present("google.genai")
        self.assertEqual(google_genai_pack.detect().matched, present)
        self.assertEqual(google_genai_pack.seams(), [])
        targets = google_genai_pack.targets()
        # Two TargetDecls (Gemini Developer API + Vertex AI), both llm:google-genai.
        self.assertEqual(len(targets), 2)
        self.assertTrue(all(t.pattern == "llm:google-genai" and t.kind == "llm" for t in targets))
        self.assertIn(google_genai_pack.HOST, targets[0].idempotency_rule)
        self.assertIn(google_genai_pack.VERTEX_HOST, targets[1].idempotency_rule)
        self.assertEqual(google_genai_pack.defaults(), {"defaults": {"llm": llm_defaults()}})

    def test_present_provider_defaults_only_lists_present(self) -> None:
        expected = [
            p.defaults()
            for p in (openai_pack, anthropic_pack, google_genai_pack)
            if p.detect().matched
        ]
        self.assertEqual(present_provider_defaults(), expected)


class VertexHostResolutionTest(unittest.TestCase):
    """Vertex AI's global + regional endpoints both route to llm:google-genai
    (Node parity: node/keel/test/judge.test.mjs; native/stub parity:
    conformance scenarios 36-38). Resolution lives on the backend since
    Task 10/SP-1 — exercised here against the pure-Python stub directly."""

    def setUp(self) -> None:
        self.backend = KeelCoreStub()

    def test_global_endpoint_exact_match(self) -> None:
        self.assertEqual(
            self.backend.resolve_target("GET", "aiplatform.googleapis.com"), "llm:google-genai"
        )

    def test_regional_endpoints_via_suffix_rule(self) -> None:
        for host in (
            "us-central1-aiplatform.googleapis.com",
            "europe-west4-aiplatform.googleapis.com",
            "asia-northeast1-aiplatform.googleapis.com",
        ):
            self.assertEqual(self.backend.resolve_target("GET", host), "llm:google-genai", host)

    def test_lookalike_host_without_hyphen_does_not_match(self) -> None:
        # Guards the suffix rule's boundary: no hyphen before "aiplatform" means
        # it is not a documented Vertex regional host.
        self.assertEqual(
            self.backend.resolve_target("GET", "evilaiplatform.googleapis.com"),
            "evilaiplatform.googleapis.com",
        )

    def test_generativelanguage_still_resolves(self) -> None:
        self.assertEqual(
            self.backend.resolve_target("GET", "generativelanguage.googleapis.com"),
            "llm:google-genai",
        )


class DevCacheArgsHashJudgeTest(unittest.TestCase):
    """The dev-cache args_hash exception, at the judge (seam) level.

    `_judge` resolves its target via `_runtime.get_backend().resolve_target`
    (Task 10/SP-1), so calling it directly (bypassing `_run_sync`/`_run_async`,
    which would otherwise short-circuit before ever reaching `_judge` when no
    backend is installed) now requires an active backend. A bare, unconfigured
    `KeelCoreStub` reproduces the exact pre-Task-10 "no backend" defaults for
    everything else these tests check (idempotency-header resolution reads
    `_policy.get("defaults", {})`, empty either way)."""

    def setUp(self) -> None:
        _runtime.set_runtime(KeelCoreStub(), None)

    def tearDown(self) -> None:
        _runtime.clear_runtime()

    def test_llm_post_gets_a_canonical_body_hash(self) -> None:
        req = httpx.Request(
            "POST",
            "https://api.openai.com/v1/chat/completions",
            json={"model": "m", "messages": [{"role": "user", "content": "hi"}]},
        )
        target, _op, idempotent, hash_, _injected = httpx_pack._judge(req)
        self.assertEqual(target, "llm:openai")
        self.assertFalse(idempotent, "an LLM POST stays non-idempotent (never retried)")
        self.assertIsNotNone(hash_)
        assert hash_ is not None
        self.assertEqual(len(hash_), 64)

    def test_llm_get_hashes_method_and_url(self) -> None:
        url = "https://api.openai.com/v1/models"
        _t, _op, _idem, hash_, _injected = httpx_pack._judge(httpx.Request("GET", url))
        self.assertEqual(hash_, _http.args_hash("GET", url))

    def test_non_llm_post_has_no_hash(self) -> None:
        req = httpx.Request("POST", "https://example.com/x", json={"a": 1})
        target, _op, idempotent, hash_, _injected = httpx_pack._judge(req)
        self.assertEqual(target, "example.com")
        self.assertFalse(idempotent)
        self.assertIsNone(hash_, "the dev-cache exception is llm-only")

    def test_httpx_and_requests_agree_on_llm_post_hash(self) -> None:
        # Canonicalization makes the two adapters' cache keys identical even when
        # they serialize the same JSON body differently.
        url = "https://api.openai.com/v1/chat/completions"
        body = {"model": "m", "messages": [{"role": "user", "content": "hi"}], "temperature": 0}
        hx = httpx_pack._judge(httpx.Request("POST", url, json=body))[3]
        prepared = PreparedRequest()
        prepared.prepare(method="POST", url=url, json=body)
        rq = requests_pack._judge(prepared)[3]
        self.assertIsNotNone(hx)
        self.assertEqual(hx, rq)

    def test_stub_cache_honors_args_hash_on_non_idempotent(self) -> None:
        # Item-2 verification: a NON-idempotent request with an args_hash replays
        # from cache (lookups don't need idempotency), and idempotent=False still
        # blocks retries on failure — no shared-semantics change required.
        core = KeelCoreStub()
        core.configure({"target": {"llm:openai": {"cache": {"ttl": DEV_CACHE_TTL}}}})
        o1 = core.execute(_req("llm:openai", "h", idempotent=False), _ok({"text": "hi"}))
        n = {"calls": 0}

        def effect(_attempt: int) -> dict[str, Any]:
            n["calls"] += 1
            return {"status": "ok", "payload": {"text": "SHOULD-NOT-RUN"}}

        o2 = core.execute(_req("llm:openai", "h", idempotent=False), effect)
        self.assertFalse(o1["from_cache"])
        self.assertTrue(o2["from_cache"])
        self.assertEqual(n["calls"], 0, "non-idempotent request still served from cache")


if __name__ == "__main__":
    unittest.main()
