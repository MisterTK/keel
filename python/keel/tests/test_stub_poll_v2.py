"""Poll v2 (CCR-8) on the Python stub, driven through the public KeelCoreStub
surface exactly as the conformance runner does. Scenarios 41–47 are the
cross-implementation pins; these are the fast, named unit twins.

`paused=True` throughout: these assert Tier 1 SEMANTICS, not pacing.
Since #119 the stub honors real durations, so an unpaused core here would
sleep out every retry/poll schedule the cases declare — minutes of wall
clock, and a deterministic surface turned into a timing-sensitive one.
"""

from __future__ import annotations

import unittest

from keel_core_stub import KeelCoreStub, KeelError

VERTEX = "us-central1-aiplatform.googleapis.com"
FETCH = "/v1/projects/p/locations/us-central1/publishers/google/models/veo-3.1:fetchPredictOperation"
SUBMIT = "/v1/projects/p/locations/us-central1/publishers/google/models/veo-3.1:predictLongRunning"
ROUTE = "POST *-aiplatform.googleapis.com/*:fetchPredictOperation"


def _poll(until: dict) -> dict:
    return {"interval": "10s", "deadline": "90s", "until": until}


def _run(core: KeelCoreStub, op: str, idempotent: bool, bodies: list) -> dict:
    script = list(bodies)
    return core.execute(
        {"v": 1, "target": "ops.internal", "op": op, "idempotent": idempotent, "args_hash": "h"},
        lambda _attempt: {"status": "ok", "payload": script.pop(0)},
    )


class RouteKeyTest(unittest.TestCase):
    def setUp(self) -> None:
        self.core = KeelCoreStub(paused=True)
        self.core.configure({"target": {ROUTE: {}, "*.googleapis.com": {}, "*-aiplatform.googleapis.com/*": {}}})

    def test_route_key_beats_the_host_map_for_its_route_only(self) -> None:
        self.assertEqual(self.core.resolve_target("POST", VERTEX, "https", None, FETCH), ROUTE)
        # A GET here still matches the second, method-agnostic route key
        # ("*-aiplatform.googleapis.com/*" has no method prefix and its path
        # glob "/*" matches anything) -- the ROUTE key's own POST-only
        # requirement doesn't disqualify OTHER route keys from route_only's
        # candidate set. Ground truth: crates/keel-core-api/src/policy.rs's
        # `resolve_target` returns this same key for the identical inputs
        # (verified against the real Rust core, not assumed).
        self.assertEqual(self.core.resolve_target("GET", VERTEX, "https", None, FETCH), "*-aiplatform.googleapis.com/*")
        self.assertEqual(self.core.resolve_target("POST", VERTEX, "https", None, SUBMIT), "*-aiplatform.googleapis.com/*")
        self.assertEqual(self.core.resolve_target("POST", "generativelanguage.googleapis.com", None, None, "/v1beta/m:generateContent"), "llm:google-genai")
        self.assertEqual(self.core.resolve_target("GET", "storage.googleapis.com", None, None, "/b/x"), "*.googleapis.com")

    def test_non_llm_exact_still_beats_route_key(self) -> None:
        core = KeelCoreStub(paused=True)
        core.configure({"target": {"api.example.com": {}, "GET api.example.com/*": {}}})
        self.assertEqual(core.resolve_target("GET", "api.example.com", None, None, "/v1/x"), "api.example.com")

    def test_class_prefixed_slash_key_does_not_enable_tier_zero(self) -> None:
        # `py:pkg/mod:fn*` contains `/` but is class-prefixed, so it is not a
        # route key: the cheap "any route key?" pre-test stays false and the
        # LLM host map still wins.
        core = KeelCoreStub(paused=True)
        core.configure({"target": {"py:pkg/mod:fn*": {}, "*.googleapis.com": {}}})
        self.assertEqual(core.resolve_target("POST", VERTEX, "https", None, FETCH), "llm:google-genai")


class TypedTerminalTest(unittest.TestCase):
    def test_boolean_terminal_matches_by_type(self) -> None:
        core = KeelCoreStub(paused=True)
        core.configure({"target": {"ops.internal": {"poll": _poll({"field": "done", "terminal": [True]})}}})
        out = _run(core, "POST ops.internal/op:fetchOperation", True, [{"done": False}, {"done": "true"}, {"done": 1}, {"done": True}])
        self.assertEqual(out["attempts"], 4)
        self.assertEqual(out["payload"], {"done": True})

    def test_numeric_terminal_matches_by_value_not_string(self) -> None:
        core = KeelCoreStub(paused=True)
        core.configure({"target": {"ops.internal": {"poll": _poll({"field": "progress", "terminal": [100]})}}})
        out = _run(core, "GET ops.internal/op", True, [{"progress": 99}, {"progress": "100"}, {"progress": True}, {"progress": 100.0}])
        self.assertEqual(out["attempts"], 4)

    def test_numeric_terminals_are_compared_in_the_f64_domain(self) -> None:
        # Numbers are compared as f64 in EVERY implementation (JSON has one
        # number type; Rust/Node have only f64), so two integers that differ
        # only beyond 2^53 are the same terminal. Documented, not accidental --
        # Python's exact int comparison is deliberately widened to match.
        core = KeelCoreStub(paused=True)
        core.configure({"target": {"ops.internal": {"poll": _poll({"field": "seq", "terminal": [9007199254740992]})}}})
        self.assertEqual(_run(core, "GET ops.internal/op", True, [{"seq": 9007199254740993}])["attempts"], 1)

    def test_inherited_attribute_names_are_missing_keys(self) -> None:
        # Scenario 48's unit twin: a field named after a host-language object
        # member must fail open, never resolve through a prototype/class.
        for field in ("constructor", "__proto__.x", "__class__.__name__"):
            core = KeelCoreStub(paused=True)
            core.configure({"target": {"ops.internal": {"poll": _poll({"field": field, "terminal": ["done"]})}}})
            self.assertEqual(_run(core, "GET ops.internal/op", True, [{"status": "running"}])["attempts"], 1, field)

    def test_out_of_f64_range_integer_is_pending_not_a_crash(self) -> None:
        core = KeelCoreStub(paused=True)
        core.configure({"target": {"ops.internal": {"poll": _poll({"field": "progress", "terminal": [100]})}}})
        huge = int("1" + "0" * 400)
        out = _run(core, "GET ops.internal/op", True, [{"progress": huge}, {"progress": 100}])
        self.assertEqual(out["attempts"], 2)  # pending, then terminal — no OverflowError

    def test_dotted_field_walks_objects_and_fails_open(self) -> None:
        core = KeelCoreStub(paused=True)
        core.configure({"target": {"ops.internal": {"poll": _poll({"field": "response.state", "terminal": ["SUCCEEDED"]})}}})
        out = _run(core, "GET ops.internal/op", True, [{"response": {"state": "RUNNING"}}, {"response": {"state": "SUCCEEDED"}}])
        self.assertEqual(out["attempts"], 2)
        for body in ({"metadata": {}}, {"response": "flat"}, {"response.state": "SUCCEEDED"}):
            self.assertEqual(_run(core, "GET ops.internal/op", True, [body])["attempts"], 1, body)

    def test_deadline_message_prints_the_dotted_field(self) -> None:
        core = KeelCoreStub(paused=True)
        core.configure({"target": {"ops.internal": {"poll": {"interval": "10s", "deadline": "25s", "until": {"field": "response.state", "terminal": ["X"]}}}}})
        out = _run(core, "GET ops.internal/op", True, [{"response": {"state": "R"}}] * 3)
        self.assertEqual(out["error"]["code"], "KEEL-E016")
        self.assertEqual(out["error"]["message"], "GET ops.internal/op poll deadline exceeded: 'response.state' not terminal after 25000ms")


class GateTest(unittest.TestCase):
    def test_gate_is_idempotency_not_method(self) -> None:
        core = KeelCoreStub(paused=True)
        core.configure({"target": {"ops.internal": {"poll": _poll({"field": "status", "terminal": ["done"]})}}})
        self.assertEqual(_run(core, "POST ops.internal/op:fetchOperation", True, [{"status": "running"}, {"status": "done"}])["attempts"], 2)
        self.assertEqual(_run(core, "POST ops.internal/op:fetchOperation", False, [{"status": "running"}])["attempts"], 1)
        self.assertEqual(_run(core, "GET ops.internal/op", False, [{"status": "running"}])["attempts"], 1)


class ValidatorTest(unittest.TestCase):
    def test_terminal_item_types(self) -> None:
        for good in (["a"], [True], [1], [1.5], ["a", False, 2]):
            KeelCoreStub(paused=True).configure({"target": {"x": {"poll": _poll({"field": "f", "terminal": good})}}})
        for bad in ([], [None], [{}], [[1]], "done"):
            with self.assertRaises(KeelError) as cm:
                KeelCoreStub(paused=True).configure({"target": {"x": {"poll": _poll({"field": "f", "terminal": bad})}}})
            self.assertEqual(cm.exception.code, "KEEL-E001")
            self.assertIn("poll.until.terminal must be a non-empty array of strings, booleans, or numbers", str(cm.exception))


if __name__ == "__main__":
    unittest.main()
