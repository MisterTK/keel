"""Unit tests for `keel.testing` (docs/recording-format.md): Recording
parsing and ReplayBackend's request-matching rule. Real end-to-end replay
(through `requests`/`py:` wrappers) is exercised manually in the gap-brief
verification; these tests pin the matching/parsing logic directly, no
network or subprocess needed."""

from __future__ import annotations

import json
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

from keel.testing import Recording, ReplayBackend, UnmatchedEffect

META = {
    "v": 1,
    "type": "meta",
    "id": "r1",
    "language": "python",
    "target": "app.py",
    "args": [],
    "started_at_ms": 0,
    "redacted_headers": [],
}


def call(seq: int, target: str, op: str, args_hash: str | None, outcome: dict) -> dict:
    return {
        "v": 1,
        "type": "call",
        "seq": seq,
        "target": target,
        "op": op,
        "idempotent": args_hash is not None,
        "args_hash": args_hash,
        "attempts": 1,
        "latency_ms": 1,
        "body_captured": True,
        "outcome": outcome,
    }


def write_recording(dirpath: Path, lines: list[dict]) -> Path:
    path = dirpath / "r.ndjson"
    path.write_text("\n".join(json.dumps(line) for line in lines) + "\n", encoding="utf-8")
    return path


class RecordingLoadTest(unittest.TestCase):
    def test_parses_meta_and_call_lines_skipping_unknown_types(self) -> None:
        with TemporaryDirectory() as d:
            path = write_recording(
                Path(d),
                [
                    META,
                    call(1, "api.example.com", "GET x", "h1", {"result": "ok", "payload": 1}),
                    {"v": 1, "type": "future-kind"},
                ],
            )
            rec = Recording.load(path)
        self.assertEqual(rec.meta["id"], "r1")
        self.assertEqual(len(rec.calls), 1)
        self.assertEqual(rec.calls[0]["target"], "api.example.com")

    def test_rejects_a_file_with_no_meta_header(self) -> None:
        with TemporaryDirectory() as d:
            path = write_recording(Path(d), [{"v": 1, "type": "call", "seq": 1}])
            with self.assertRaisesRegex(ValueError, "no meta header"):
                Recording.load(path)

    def test_rejects_an_empty_file(self) -> None:
        with TemporaryDirectory() as d:
            path = Path(d) / "empty.ndjson"
            path.write_text("", encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "is empty"):
                Recording.load(path)


class ReplayBackendTest(unittest.TestCase):
    def test_matches_by_target_and_args_hash_when_present(self) -> None:
        outcome = {"result": "ok", "payload": {"x": 1}, "from_cache": False}
        rec = Recording(META, [call(1, "api.example.com", "GET x", "h1", outcome)])
        backend = ReplayBackend(rec)
        got = backend.execute({"target": "api.example.com", "op": "GET x", "args_hash": "h1"}, lambda a: None)
        # from_cache is forced True on a served "ok" (dedicated test below).
        self.assertEqual(got, {**outcome, "from_cache": True})

    def test_falls_back_to_target_and_op_when_args_hash_is_null_on_both_sides(self) -> None:
        outcome = {"result": "error", "error": {"code": "KEEL-E010"}}
        rec = Recording(META, [call(1, "api.example.com", "POST y", None, outcome)])
        backend = ReplayBackend(rec)
        got = backend.execute({"target": "api.example.com", "op": "POST y", "args_hash": None}, lambda a: None)
        self.assertEqual(got, outcome)

    def test_serves_repeated_identical_calls_in_recorded_fifo_order(self) -> None:
        first = {"result": "ok", "payload": 1, "from_cache": False}
        second = {"result": "ok", "payload": 2, "from_cache": False}
        rec = Recording(
            META,
            [
                call(1, "api.example.com", "GET x", "h1", first),
                call(2, "api.example.com", "GET x", "h1", second),
            ],
        )
        backend = ReplayBackend(rec)
        req = {"target": "api.example.com", "op": "GET x", "args_hash": "h1"}
        self.assertEqual(backend.execute(req, lambda a: None), {**first, "from_cache": True})
        self.assertEqual(backend.execute(req, lambda a: None), {**second, "from_cache": True})

    def test_raises_unmatched_effect_on_a_novel_call(self) -> None:
        rec = Recording(META, [call(1, "api.example.com", "GET x", "h1", {"result": "ok"})])
        backend = ReplayBackend(rec)
        with self.assertRaises(UnmatchedEffect):
            backend.execute({"target": "api.example.com", "op": "GET x", "args_hash": "different"}, lambda a: None)

    def test_raises_once_a_groups_recorded_calls_are_exhausted(self) -> None:
        rec = Recording(META, [call(1, "api.example.com", "GET x", "h1", {"result": "ok"})])
        backend = ReplayBackend(rec)
        req = {"target": "api.example.com", "op": "GET x", "args_hash": "h1"}
        backend.execute(req, lambda a: None)
        with self.assertRaises(UnmatchedEffect):
            backend.execute(req, lambda a: None)

    def test_never_invokes_the_callers_real_effect(self) -> None:
        rec = Recording(META, [call(1, "api.example.com", "GET x", "h1", {"result": "ok"})])
        backend = ReplayBackend(rec)
        ran = []
        backend.execute({"target": "api.example.com", "op": "GET x", "args_hash": "h1"}, lambda a: ran.append(a))
        self.assertEqual(ran, [])

    def test_leaves_an_error_outcome_untouched(self) -> None:
        error_outcome = {"result": "error", "error": {"code": "KEEL-E010"}}
        rec = Recording(META, [call(1, "api.example.com", "POST y", None, error_outcome)])
        backend = ReplayBackend(rec)
        got = backend.execute({"target": "api.example.com", "op": "POST y", "args_hash": None}, lambda a: None)
        self.assertEqual(got, error_outcome)

    def test_layer_and_configure_and_report_are_harmless_no_ops(self) -> None:
        backend = ReplayBackend(Recording(META, []))
        self.assertIsNone(backend.layer("x", "y"))
        self.assertIsNone(backend.configure({}))
        self.assertEqual(backend.report(), {})


if __name__ == "__main__":
    unittest.main()
