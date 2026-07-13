"""Unit tests for `keel._record` (docs/recording-format.md): the
RecordingBackend tee, redaction, and body-captured detection — all against a
fake `Backend`, no subprocess/network needed. The real end-to-end capture
(through `keel run`, both `py:` targets and HTTP adapters) is exercised by
`test_record_capture.py`."""

from __future__ import annotations

import json
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

from keel._record import (
    DEFAULT_REDACT_HEADERS,
    RecordingBackend,
    install_recording,
    redact_headers_from_env,
)


class FakeBackend:
    def __init__(self, outcomes: list[dict]) -> None:
        self.outcomes = list(outcomes)
        self.calls: list[dict] = []
        self.configured = None

    def configure(self, policy):
        self.configured = policy

    def execute(self, request, effect):
        self.calls.append(request)
        effect(1)  # a real call happened; recording must never suppress this
        return self.outcomes.pop(0)

    def report(self):
        return {"reported": True}


def read_lines(path: Path) -> list[dict]:
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines() if line.strip()]


class RedactHeadersTest(unittest.TestCase):
    def test_merges_env_with_defaults(self) -> None:
        redact = redact_headers_from_env({"KEEL_RECORD_REDACT_HEADERS": "x-custom, X-Another"})
        self.assertIn("authorization", redact)
        self.assertIn("x-custom", redact)
        self.assertIn("x-another", redact)
        self.assertEqual(len(redact), len(DEFAULT_REDACT_HEADERS) + 2)

    def test_empty_env_is_just_the_defaults(self) -> None:
        self.assertEqual(redact_headers_from_env({}), DEFAULT_REDACT_HEADERS)


class RecordingBackendTest(unittest.TestCase):
    def test_execute_forwards_unchanged_and_the_real_effect_actually_runs(self) -> None:
        outcome = {"v": 1, "result": "ok", "payload": {"hi": 1}, "attempts": 1, "from_cache": False}
        inner = FakeBackend([outcome])
        ran = []

        class Writer:
            def write_call(self, **kwargs):
                self.kwargs = kwargs

        writer = Writer()
        rb = RecordingBackend(inner, writer, DEFAULT_REDACT_HEADERS)
        request = {"v": 1, "target": "api.example.com", "op": "GET x", "idempotent": True, "args_hash": "h1"}
        result = rb.execute(request, lambda attempt: ran.append(attempt) or {"status": "ok", "payload": {"hi": 1}})

        self.assertIs(result, outcome, "the real backend's outcome is returned unchanged")
        self.assertEqual(ran, [1], "the real effect actually ran exactly once")
        self.assertEqual(inner.calls, [request])
        self.assertEqual(writer.kwargs["target"], "api.example.com")
        self.assertEqual(writer.kwargs["args_hash"], "h1")

    def test_delegates_configure_and_report(self) -> None:
        inner = FakeBackend([])
        rb = RecordingBackend(inner, None, DEFAULT_REDACT_HEADERS)
        rb.configure({"target": {}})
        self.assertEqual(inner.configured, {"target": {}})
        self.assertEqual(rb.report(), {"reported": True})

    def test_getattr_delegates_unknown_attributes(self) -> None:
        inner = FakeBackend([])
        inner.layer = lambda target, key: "resolved"
        rb = RecordingBackend(inner, None, DEFAULT_REDACT_HEADERS)
        self.assertEqual(rb.layer("t", "k"), "resolved")

    def test_getattr_raises_for_a_truly_missing_attribute(self) -> None:
        inner = FakeBackend([])
        rb = RecordingBackend(inner, None, DEFAULT_REDACT_HEADERS)
        with self.assertRaises(AttributeError):
            rb.definitely_not_a_real_attribute  # noqa: B018


class InstallRecordingTest(unittest.TestCase):
    def test_writes_meta_then_one_call_line_per_execute_and_redacts_auth_header(self) -> None:
        outcome = {
            "v": 1,
            "result": "ok",
            "payload": {
                "__keel_http__": 1,
                "status": 200,
                "headers": [["content-type", "application/json"], ["Authorization", "Bearer secret"]],
                "body_b64": "eyJvayI6dHJ1ZX0=",
            },
            "attempts": 1,
            "from_cache": False,
        }
        inner = FakeBackend([outcome])
        with TemporaryDirectory() as d:
            path = Path(d) / "0000000000001-0000.ndjson"
            rb = install_recording(
                inner, path=str(path), target="app.py", args=["--flag"], env={"KEEL_QUIET": "1"}
            )
            request = {"v": 1, "target": "api.example.com", "op": "GET api.example.com/x", "idempotent": True, "args_hash": "abc"}
            rb.execute(request, lambda attempt: {"status": "ok", "payload": {}})

            lines = read_lines(path)
        self.assertEqual(len(lines), 2)
        meta, call = lines
        self.assertEqual(meta["type"], "meta")
        self.assertEqual(meta["id"], "0000000000001-0000")
        self.assertEqual(meta["language"], "python")
        self.assertEqual(meta["target"], "app.py")
        self.assertEqual(meta["args"], ["--flag"])
        self.assertEqual(sorted(meta["redacted_headers"]), sorted(DEFAULT_REDACT_HEADERS))

        self.assertEqual(call["type"], "call")
        self.assertEqual(call["seq"], 1)
        self.assertEqual(call["target"], "api.example.com")
        self.assertEqual(call["args_hash"], "abc")
        self.assertTrue(call["body_captured"])
        headers = dict(call["outcome"]["payload"]["headers"])
        self.assertEqual(headers["Authorization"], "[REDACTED]")
        self.assertEqual(headers["content-type"], "application/json", "non-secret headers pass through")

    def test_body_captured_is_false_with_no_buffered_body(self) -> None:
        outcome = {
            "v": 1,
            "result": "error",
            "error": {"code": "KEEL-E010", "class": "http", "message": "HTTP 503"},
            "attempts": 3,
            "from_cache": False,
        }
        inner = FakeBackend([outcome])
        with TemporaryDirectory() as d:
            path = Path(d) / "r.ndjson"
            rb = install_recording(inner, path=str(path), target="app.py", args=[], env={"KEEL_QUIET": "1"})
            rb.execute(
                {"v": 1, "target": "api.example.com", "op": "POST x/y", "idempotent": False, "args_hash": None},
                lambda attempt: {"status": "error", "class": "http", "http_status": 503, "message": "HTTP 503"},
            )
            lines = read_lines(path)
        self.assertFalse(lines[1]["body_captured"])
        self.assertEqual(lines[1]["outcome"]["result"], "error")

    def test_a_python_return_value_is_captured_as_body_when_present(self) -> None:
        outcome = {"v": 1, "result": "ok", "payload": {"value": 42}, "attempts": 1, "from_cache": False}
        inner = FakeBackend([outcome])
        with TemporaryDirectory() as d:
            path = Path(d) / "r.ndjson"
            rb = install_recording(inner, path=str(path), target="app.py", args=[], env={"KEEL_QUIET": "1"})
            rb.execute(
                {"v": 1, "target": "py:lib.fn", "op": "py:lib.fn", "idempotent": True, "args_hash": "h1"},
                lambda attempt: {"status": "ok", "payload": {"value": 42}},
            )
            lines = read_lines(path)
        self.assertTrue(lines[1]["body_captured"])
        self.assertEqual(lines[1]["outcome"]["payload"], {"value": 42})


if __name__ == "__main__":
    unittest.main()
