"""Poll v2 (CCR-8) on the Python stub, driven through the public KeelCoreStub
surface exactly as the conformance runner does. Scenarios 41–47 are the
cross-implementation pins; these are the fast, named unit twins."""

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
        self.core = KeelCoreStub()
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
        core = KeelCoreStub()
        core.configure({"target": {"api.example.com": {}, "GET api.example.com/*": {}}})
        self.assertEqual(core.resolve_target("GET", "api.example.com", None, None, "/v1/x"), "api.example.com")


class TypedTerminalTest(unittest.TestCase):
    def test_boolean_terminal_matches_by_type(self) -> None:
        core = KeelCoreStub()
        core.configure({"target": {"ops.internal": {"poll": _poll({"field": "done", "terminal": [True]})}}})
        out = _run(core, "POST ops.internal/op:fetchOperation", True, [{"done": False}, {"done": "true"}, {"done": 1}, {"done": True}])
        self.assertEqual(out["attempts"], 4)
        self.assertEqual(out["payload"], {"done": True})

    def test_numeric_terminal_matches_by_value_not_string(self) -> None:
        core = KeelCoreStub()
        core.configure({"target": {"ops.internal": {"poll": _poll({"field": "progress", "terminal": [100]})}}})
        out = _run(core, "GET ops.internal/op", True, [{"progress": 99}, {"progress": "100"}, {"progress": True}, {"progress": 100.0}])
        self.assertEqual(out["attempts"], 4)

    def test_dotted_field_walks_objects_and_fails_open(self) -> None:
        core = KeelCoreStub()
        core.configure({"target": {"ops.internal": {"poll": _poll({"field": "response.state", "terminal": ["SUCCEEDED"]})}}})
        out = _run(core, "GET ops.internal/op", True, [{"response": {"state": "RUNNING"}}, {"response": {"state": "SUCCEEDED"}}])
        self.assertEqual(out["attempts"], 2)
        for body in ({"metadata": {}}, {"response": "flat"}, {"response.state": "SUCCEEDED"}):
            self.assertEqual(_run(core, "GET ops.internal/op", True, [body])["attempts"], 1, body)

    def test_deadline_message_prints_the_dotted_field(self) -> None:
        core = KeelCoreStub()
        core.configure({"target": {"ops.internal": {"poll": {"interval": "10s", "deadline": "25s", "until": {"field": "response.state", "terminal": ["X"]}}}}})
        out = _run(core, "GET ops.internal/op", True, [{"response": {"state": "R"}}] * 3)
        self.assertEqual(out["error"]["code"], "KEEL-E016")
        self.assertEqual(out["error"]["message"], "GET ops.internal/op poll deadline exceeded: 'response.state' not terminal after 25000ms")


class GateTest(unittest.TestCase):
    def test_gate_is_idempotency_not_method(self) -> None:
        core = KeelCoreStub()
        core.configure({"target": {"ops.internal": {"poll": _poll({"field": "status", "terminal": ["done"]})}}})
        self.assertEqual(_run(core, "POST ops.internal/op:fetchOperation", True, [{"status": "running"}, {"status": "done"}])["attempts"], 2)
        self.assertEqual(_run(core, "POST ops.internal/op:fetchOperation", False, [{"status": "running"}])["attempts"], 1)
        self.assertEqual(_run(core, "GET ops.internal/op", False, [{"status": "running"}])["attempts"], 1)


class ValidatorTest(unittest.TestCase):
    def test_terminal_item_types(self) -> None:
        for good in (["a"], [True], [1], [1.5], ["a", False, 2]):
            KeelCoreStub().configure({"target": {"x": {"poll": _poll({"field": "f", "terminal": good})}}})
        for bad in ([], [None], [{}], [[1]], "done"):
            with self.assertRaises(KeelError) as cm:
                KeelCoreStub().configure({"target": {"x": {"poll": _poll({"field": "f", "terminal": bad})}}})
            self.assertEqual(cm.exception.code, "KEEL-E001")
            self.assertIn("poll.until.terminal must be a non-empty array of strings, booleans, or numbers", str(cm.exception))


if __name__ == "__main__":
    unittest.main()
