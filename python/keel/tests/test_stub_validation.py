"""Python stub configure() validation parity with the native core / frozen schema.

Three drifts the whole-branch review flagged, now closed on the pure-Python stub:
  * unknown/typo'd policy keys are rejected with KEEL-E001 + a path (the core's
    deny_unknown_fields; previously the stub silently ran the defaults);
  * cache scope/mode/key are closed enums (a typo like scope="persistant" fails);
  * numeric literals are ASCII-only with no embedded whitespace, and schedule
    factors reject inf/nan/underscore (Python's str.isdigit()/float() accepted
    unicode digits, "3 / s", "xinf", "x1_0" — Node and the core reject them).

Valid policies (including the front end's own Level 0 output) must still pass.
"""

from __future__ import annotations

import unittest

from keel_core_stub import KeelCoreStub, KeelError


class UnknownKeyRejectionTest(unittest.TestCase):
    def _rejects(self, policy: dict) -> str:
        with self.assertRaises(KeelError) as ctx:
            KeelCoreStub().configure(policy)
        self.assertEqual(ctx.exception.code, "KEEL-E001")
        return ctx.exception.message

    def test_unknown_top_level_key(self) -> None:
        self.assertIn("bogus", self._rejects({"bogus": True}))

    def test_unknown_target_layer_key(self) -> None:
        # `retrys` (typo) — the exact silent-surprise the schema was written to stop.
        self._rejects({"target": {"api.stripe.com": {"retrys": {"attempts": 10}}}})

    def test_unknown_retry_key(self) -> None:
        self._rejects({"target": {"api.stripe.com": {"retry": {"atempts": 10}}}})

    def test_unknown_cache_key(self) -> None:
        self._rejects({"target": {"x": {"cache": {"ttl": "10m", "expiry": "1h"}}}})

    def test_unknown_defaults_key(self) -> None:
        self._rejects({"defaults": {"inbound": {}}})


class CacheEnumStrictnessTest(unittest.TestCase):
    def _rejects(self, cache: dict) -> None:
        with self.assertRaises(KeelError) as ctx:
            KeelCoreStub().configure({"target": {"x": {"cache": cache}}})
        self.assertEqual(ctx.exception.code, "KEEL-E001")

    def test_bad_scope(self) -> None:
        self._rejects({"ttl": "10m", "scope": "persistant"})

    def test_bad_mode(self) -> None:
        self._rejects({"ttl": "10m", "mode": "development"})

    def test_bad_key(self) -> None:
        self._rejects({"ttl": "10m", "key": "body"})

    def test_valid_enums_accepted(self) -> None:
        KeelCoreStub().configure(
            {"target": {"x": {"cache": {"ttl": "10m", "scope": "persistent", "mode": "dev", "key": "url"}}}}
        )


class NumericLiteralParityTest(unittest.TestCase):
    def _rejects(self, tp: dict) -> None:
        with self.assertRaises(KeelError) as ctx:
            KeelCoreStub().configure({"target": {"x": tp}})
        self.assertEqual(ctx.exception.code, "KEEL-E001")

    def test_rate_with_internal_whitespace(self) -> None:
        self._rejects({"rate": "3 / s"})

    def test_duration_unicode_digits(self) -> None:
        self._rejects({"timeout": "３０s"})  # fullwidth digits

    def test_schedule_factor_inf_nan_underscore(self) -> None:
        for factor in ("xinf", "xnan", "x1_0"):
            self._rejects({"retry": {"schedule": f"exp(1s, {factor})"}})

    def test_well_formed_values_accepted(self) -> None:
        KeelCoreStub().configure(
            {"target": {"x": {"rate": "3/s", "timeout": "30s", "retry": {"schedule": "exp(200ms, x2, max 30s)"}}}}
        )


class BreakerRateModeValidationTest(unittest.TestCase):
    """Breaker rate mode (window + failure_rate + min_calls), parity with the
    real core's `BreakerPolicy` (crates/keel-core-api/src/policy.rs): a
    rate-mode knob without both `window` and `failure_rate` present (and
    without `failures`) is KEEL-E001 at configure time, not a silent
    degrade to count mode on its defaults."""

    def _rejects(self, breaker: dict) -> None:
        with self.assertRaises(KeelError) as ctx:
            KeelCoreStub().configure({"target": {"x": {"breaker": breaker}}})
        self.assertEqual(ctx.exception.code, "KEEL-E001")

    def test_window_alone_is_half_configured(self) -> None:
        self._rejects({"window": "30s"})

    def test_failure_rate_alone_is_half_configured(self) -> None:
        self._rejects({"failure_rate": 0.5})

    def test_min_calls_alone_is_half_configured(self) -> None:
        self._rejects({"min_calls": 4})

    def test_out_of_range_failure_rate_rejected(self) -> None:
        for rate in (0, -0.1, 1.1, 2):
            self._rejects({"window": "30s", "failure_rate": rate})

    def test_non_positive_min_calls_rejected(self) -> None:
        self._rejects({"window": "30s", "failure_rate": 0.5, "min_calls": 0})

    def test_both_rate_knobs_selects_rate_mode(self) -> None:
        KeelCoreStub().configure(
            {"target": {"x": {"breaker": {"window": "30s", "failure_rate": 0.5, "min_calls": 4}}}}
        )

    def test_failures_alongside_rate_knobs_is_still_valid_count_mode(self) -> None:
        # Frozen schema precedence: "Setting `failures` selects count mode" —
        # the rate knobs are inert, not rejected.
        KeelCoreStub().configure(
            {"target": {"x": {"breaker": {"failures": 3, "window": "30s", "failure_rate": 0.5}}}}
        )


class ValidTopLevelSectionsTest(unittest.TestCase):
    def test_flows_journal_telemetry_and_idempotency_accepted(self) -> None:
        KeelCoreStub().configure(
            {
                "flows": {"entrypoints": ["py:m:f"], "on_nondeterminism": "warn"},
                "journal": "file:.keel/journal.db",
                "telemetry": {"otlp_endpoint": "http://x:4317", "console": False},
                "target": {"api.stripe.com": {"idempotency": {"header": "X-Request-Token"}}},
            }
        )

    def test_bad_journal_and_flows_enum_rejected(self) -> None:
        for bad in ({"journal": "sqlite:x"}, {"flows": {"on_nondeterminism": "explode"}}):
            with self.assertRaises(KeelError) as ctx:
                KeelCoreStub().configure(bad)
            self.assertEqual(ctx.exception.code, "KEEL-E001")

    def test_scenario15_still_rejected(self) -> None:
        # Conformance scenario 15 (value error, not an unknown key) stays E001.
        with self.assertRaises(KeelError) as ctx:
            KeelCoreStub().configure({"target": {"api.example.com": {"retry": {"attempts": 0}}}})
        self.assertEqual(ctx.exception.code, "KEEL-E001")


if __name__ == "__main__":
    unittest.main()
