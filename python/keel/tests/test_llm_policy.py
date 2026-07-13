"""Unit tests for the LLM budget-cap + fallback-chain helpers (_llm_policy.py).

Seam-level (httpx_pack / requests_pack) integration is covered in
test_llm_budget_fallback.py. This is the Python twin of
node/keel/test/llm-policy.test.mjs — keep the two mirrored.
"""

from __future__ import annotations

import json
import unittest

from keel.adapters import _llm_policy as lp


class ParseBudgetCentsTest(unittest.TestCase):
    def test_accepts_the_frozen_grammar_only(self) -> None:
        self.assertEqual(lp.parse_budget_cents("$5/run"), 500)
        self.assertEqual(lp.parse_budget_cents("$5.50/run"), 550)
        self.assertEqual(lp.parse_budget_cents("$0.01/run"), 1)
        self.assertIsNone(lp.parse_budget_cents("5/run"))
        self.assertIsNone(lp.parse_budget_cents("$5/day"))
        self.assertIsNone(lp.parse_budget_cents("100 tokens/run"))
        self.assertIsNone(lp.parse_budget_cents(None))
        self.assertIsNone(lp.parse_budget_cents(""))


class NormalizeUsageTest(unittest.TestCase):
    def test_handles_openai_anthropic_and_google_shapes(self) -> None:
        self.assertEqual(
            lp.normalize_usage({"usage": {"prompt_tokens": 10, "completion_tokens": 20}}),
            {"input_tokens": 10, "output_tokens": 20},
        )
        self.assertEqual(
            lp.normalize_usage({"usage": {"input_tokens": 5, "output_tokens": 7}}),
            {"input_tokens": 5, "output_tokens": 7},
        )
        self.assertEqual(
            lp.normalize_usage({"usageMetadata": {"promptTokenCount": 3, "candidatesTokenCount": 4}}),
            {"input_tokens": 3, "output_tokens": 4},
        )
        self.assertIsNone(lp.normalize_usage({"reply": "hi"}))
        self.assertIsNone(lp.normalize_usage(None))


class EstimateCostUsdTest(unittest.TestCase):
    def test_prices_known_models_and_falls_back_for_unknown(self) -> None:
        usage = {"input_tokens": 1_000_000, "output_tokens": 1_000_000}
        self.assertAlmostEqual(lp.estimate_cost_usd("gpt-4o-mini", usage), 0.15 + 0.6)
        self.assertAlmostEqual(lp.estimate_cost_usd("gpt-4o-mini-2026-01-01", usage), 0.15 + 0.6)
        self.assertNotAlmostEqual(lp.estimate_cost_usd("gpt-4o-mini", usage), lp.estimate_cost_usd("gpt-4o", usage))
        self.assertAlmostEqual(lp.estimate_cost_usd("some-future-model-nobody-has-priced-yet", usage), 10 + 30)
        self.assertEqual(lp.estimate_cost_usd("gpt-4o", None), 0.0)


class LedgerTest(unittest.TestCase):
    def setUp(self) -> None:
        lp.reset_llm_budgets()

    def tearDown(self) -> None:
        lp.reset_llm_budgets()

    def test_accumulates_per_target_and_resets(self) -> None:
        self.assertEqual(lp.spent_cents("llm:openai"), 0)
        lp.record_spend("llm:openai", 1.2345)
        lp.record_spend("llm:openai", 0.5)
        self.assertEqual(lp.spent_cents("llm:openai"), 173)
        self.assertEqual(lp.spent_cents("llm:anthropic"), 0)
        lp.reset_llm_budgets()
        self.assertEqual(lp.spent_cents("llm:openai"), 0)


class BudgetMessageTest(unittest.TestCase):
    def test_carries_human_what_why_next_and_keel_e012(self) -> None:
        msg = lp.budget_message("llm:openai", 500, 512)
        self.assertIn("$5.00/run", msg)
        self.assertIn("$5.12", msg)
        self.assertIn("budget", msg.lower())
        outcome = lp.budget_blocked_outcome(msg)
        self.assertEqual(outcome["result"], "error")
        self.assertEqual(outcome["attempts"], 0)
        self.assertEqual(outcome["breaker"], "open")
        self.assertEqual(outcome["error"]["code"], "KEEL-E012")
        self.assertEqual(outcome["error"]["message"], msg)


class DeriveAndRewriteModelTest(unittest.TestCase):
    def test_derive_reads_json_body_or_google_url(self) -> None:
        self.assertEqual(
            lp.derive_request_model(
                "https://api.openai.com/v1/chat/completions", json.dumps({"model": "gpt-4o"})
            ),
            "gpt-4o",
        )
        self.assertEqual(
            lp.derive_request_model(
                "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-pro:generateContent",
                json.dumps({"contents": []}),
            ),
            "gemini-2.5-pro",
        )
        self.assertIsNone(lp.derive_request_model("https://api.example.com/x", "not json"))
        self.assertIsNone(lp.derive_request_model("https://api.example.com/x", None))

    def test_rewrite_swaps_json_body_model_field(self) -> None:
        r = lp.rewrite_model(
            "https://api.openai.com/v1/chat/completions",
            json.dumps({"model": "gpt-4o", "temperature": 0}),
            "gpt-4o-mini",
        )
        assert r is not None
        url, body = r
        self.assertEqual(url, "https://api.openai.com/v1/chat/completions")
        self.assertEqual(json.loads(body), {"model": "gpt-4o-mini", "temperature": 0})

    def test_rewrite_preserves_bytes_body_type(self) -> None:
        r = lp.rewrite_model(
            "https://api.openai.com/v1/chat/completions",
            json.dumps({"model": "gpt-4o"}).encode("utf-8"),
            "gpt-4o-mini",
        )
        assert r is not None
        _url, body = r
        self.assertIsInstance(body, (bytes, bytearray))
        self.assertEqual(json.loads(body), {"model": "gpt-4o-mini"})

    def test_rewrite_swaps_google_url_path_segment(self) -> None:
        r = lp.rewrite_model(
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-pro:generateContent",
            json.dumps({"contents": []}),
            "gemini-2.5-flash",
        )
        assert r is not None
        url, body = r
        self.assertEqual(
            url, "https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-flash:generateContent"
        )
        self.assertEqual(body, json.dumps({"contents": []}))

    def test_rewrite_returns_none_for_unrecognized_shape(self) -> None:
        self.assertIsNone(lp.rewrite_model("https://api.example.com/x", "not json, no model field", "other"))
        self.assertIsNone(lp.rewrite_model("https://api.example.com/x", None, "other"))


class ShouldFallbackTest(unittest.TestCase):
    def test_chases_any_terminal_failure_except_breaker_open(self) -> None:
        self.assertTrue(lp.should_fallback({"code": "KEEL-E010"}))
        self.assertTrue(lp.should_fallback({"code": "KEEL-E014"}))
        self.assertTrue(lp.should_fallback({"code": "KEEL-E015"}))
        self.assertFalse(lp.should_fallback({"code": "KEEL-E012"}))
        self.assertFalse(lp.should_fallback(None))


if __name__ == "__main__":
    unittest.main()
