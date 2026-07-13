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


class ScheduleCompositionTest(unittest.TestCase):
    """upTo/andThen composition (contracts/schedule-grammar.ebnf), semantics
    normatively pinned in conformance/README.md ("Schedule algebra")."""

    def _rejects(self, schedule: str) -> None:
        with self.assertRaises(KeelError) as ctx:
            KeelCoreStub().configure({"target": {"x": {"retry": {"schedule": schedule}}}})
        self.assertEqual(ctx.exception.code, "KEEL-E001")

    def _waits(self, schedule: str, attempts: int) -> list[int]:
        core = KeelCoreStub()
        core.configure(
            {
                "target": {
                    "x": {"retry": {"attempts": attempts, "schedule": schedule, "on": ["timeout"]}}
                }
            }
        )
        out = core.execute(
            {"v": 1, "target": "x", "op": "x", "idempotent": True},
            lambda attempt: {"status": "error", "class": "timeout"},
        )
        self.assertEqual(out["attempts"], attempts)
        return out["waits_ms"]

    def test_spec_example_parses(self) -> None:
        KeelCoreStub().configure(
            {"target": {"x": {"retry": {"schedule": "exp(1s, x2, max 5m) upTo 10m andThen fixed(1m)"}}}}
        )

    def test_handoff_when_the_bound_would_be_overshot(self) -> None:
        # 1s + 2s = 3s fits; the natural 4s would overshoot the 4s bound.
        waits = self._waits("exp(1s, x2) upTo 4s andThen fixed(500ms)", 6)
        self.assertEqual(waits, [1000, 2000, 500, 500, 500])

    def test_exact_fit_stays_and_cascade_skips_a_segment(self) -> None:
        # Three 1s waits fill `upTo 3s` exactly; `fixed(10s) upTo 5s`'s own
        # first wait already exceeds its bound, so it contributes nothing and
        # the handoff cascades straight to the fixed(250ms) tail.
        waits = self._waits(
            "fixed(1s) upTo 3s andThen fixed(10s) upTo 5s andThen fixed(250ms)", 7
        )
        self.assertEqual(waits, [1000, 1000, 1000, 250, 250, 250])

    def test_exp_restarts_at_local_attempt_1_after_a_handoff(self) -> None:
        # attempts=6 so all 5 waits (through the 6th, terminal attempt) show up.
        waits = self._waits("fixed(1s) upTo 2s andThen exp(100ms, x3)", 6)
        self.assertEqual(waits, [1000, 1000, 100, 300, 900])

    def test_shape_rule_rejects_degenerate_composition(self) -> None:
        for degenerate in (
            "fixed(1s) andThen fixed(2s)",  # non-final segment unbounded: never hands off
            "exp(1s, x2, max 5m) upTo 10m",  # final segment bounded: attempts past it have no wait
            "fixed(1s) upTo 3s andThen fixed(2s) andThen fixed(4s)",
            "fixed(1s) upTo 3s andThen fixed(2s) upTo 5s",
        ):
            self._rejects(degenerate)

    def test_broken_composition_syntax_rejected(self) -> None:
        for broken in (
            "fixed(1s) upTo",
            "upTo 3s andThen fixed(1s)",
            "fixed(1s) upTo 1s upTo 2s andThen fixed(1s)",
            "fixed(1s) andThen",
            "andThen fixed(1s)",
            "fixed(1s) upTo 3s fixed(2s)",
        ):
            self._rejects(broken)


if __name__ == "__main__":
    unittest.main()
