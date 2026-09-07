"""keel._summary — the exit-time console summary (design spec Part A).

Unit tests for the counter classification and the formatter, driven by the
shared corpus in conformance/console_summary/ (Node consumes the same files,
so wording cannot drift between front ends). Subprocess tests live in Task 2.
"""

from __future__ import annotations

import json
import unittest
from pathlib import Path

from . import REPO_ROOT

CORPUS = REPO_ROOT / "conformance" / "console_summary"


def _outcome(**overrides):
    base = {
        "v": 1,
        "result": "ok",
        "attempts": 1,
        "from_cache": False,
        "waits_ms": [],
        "throttled": False,
        "throttle_wait_ms": 0,
        "breaker": "closed",
        "trace_id": "t-000001",
    }
    base.update(overrides)
    return base


class SummaryCountsTest(unittest.TestCase):
    def test_plain_success_counts_one_call(self) -> None:
        from keel._summary import Summary

        s = Summary()
        s.observe(_outcome(), wrapped=True)
        self.assertEqual(
            s.counts(),
            {"calls": 1, "throttled": 0, "retries_succeeded": 0, "breaker_trips": 0,
             "cache_hits": 0, "not_retried": 0, "unprotected": 0},
        )

    def test_retry_succeeded_requires_ok_and_attempts_gt_1(self) -> None:
        from keel._summary import Summary

        s = Summary()
        s.observe(_outcome(attempts=3), wrapped=True)  # counts
        s.observe(_outcome(result="error", attempts=3, error={"code": "KEEL-E010"}), wrapped=True)  # does not
        s.observe(_outcome(attempts=2, from_cache=True), wrapped=True)  # cache hit, not a retry
        self.assertEqual(s.counts()["retries_succeeded"], 1)
        self.assertEqual(s.counts()["cache_hits"], 1)

    def test_error_codes_classify_breaker_and_not_retried(self) -> None:
        from keel._summary import Summary

        s = Summary()
        s.observe(_outcome(result="error", attempts=0, error={"code": "KEEL-E012"}), wrapped=True)
        s.observe(_outcome(result="error", attempts=1, error={"code": "KEEL-E014"}), wrapped=True)
        self.assertEqual(s.counts()["breaker_trips"], 1)
        self.assertEqual(s.counts()["not_retried"], 1)

    def test_throttled_and_unwrapped(self) -> None:
        from keel._summary import Summary

        s = Summary()
        s.observe(_outcome(throttled=True), wrapped=False)
        self.assertEqual(s.counts()["throttled"], 1)
        self.assertEqual(s.counts()["unprotected"], 1)


class SummaryFormatCorpusTest(unittest.TestCase):
    def test_corpus_present(self) -> None:
        self.assertGreaterEqual(len(sorted(CORPUS.glob("*.json"))), 5)

    def test_every_corpus_case_formats_byte_identically(self) -> None:
        from keel._summary import format_summary

        for path in sorted(CORPUS.glob("*.json")):
            case = json.loads(path.read_text(encoding="utf-8"))
            with self.subTest(case=case["name"]):
                self.assertEqual(format_summary(case["counts"], case["keel_on_path"]), case["expected"])


class KeelOnPathTest(unittest.TestCase):
    def test_uses_shutil_which(self) -> None:
        import shutil
        from unittest import mock

        from keel import _summary

        with mock.patch.object(shutil, "which", return_value="/usr/local/bin/keel"):
            self.assertTrue(_summary.keel_on_path())
        with mock.patch.object(shutil, "which", return_value=None):
            self.assertFalse(_summary.keel_on_path())
