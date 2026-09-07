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


import re
import subprocess
import sys
from tempfile import TemporaryDirectory

from . import FIXTURES, child_env

ENRICH = str(FIXTURES / "enrich_app.py")
NOOP = str(FIXTURES / "noop_app.py")
TARGET_POLICY = '[target."py:sample_targets.enrich_*"]\n'
# `keel` may or may not be on PATH on the machine running the suite; either
# bridge form is correct, so match both.
SUMMARY_RE = re.compile(
    r"keel ▸ 1 call\n {7}(uvx --from keelrun-cli )?keel report --open for the full picture\n"
)


def _run(cmd: list[str], *, env: dict[str, str], cwd: str) -> subprocess.CompletedProcess[bytes]:
    return subprocess.run(cmd, env=env, cwd=cwd, capture_output=True)


class SummaryAtExitTest(unittest.TestCase):
    def _run_enrich(self, policy: str, **env: str) -> subprocess.CompletedProcess[bytes]:
        with TemporaryDirectory() as d:
            (Path(d) / "keel.toml").write_text(policy, encoding="utf-8")
            return _run([sys.executable, "-m", "keel", "run", ENRICH], env=child_env(**env), cwd=d)

    def test_summary_printed_on_stderr_after_one_wrapped_call(self) -> None:
        proc = self._run_enrich(TARGET_POLICY)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertEqual(proc.stdout, b"enriched 42\n")  # stdout untouched
        err = proc.stderr.decode("utf-8")
        self.assertIn("keel ▸ wrapped", err)  # banner first
        self.assertRegex(err, SUMMARY_RE)
        self.assertLess(err.index("wrapped"), err.index("report --open"))

    def test_zero_activity_run_prints_no_summary(self) -> None:
        with TemporaryDirectory() as d:
            (Path(d) / "keel.toml").write_text(TARGET_POLICY, encoding="utf-8")
            proc = _run([sys.executable, "-m", "keel", "run", NOOP], env=child_env(), cwd=d)
        self.assertNotIn(b"report --open", proc.stderr)

    def test_console_false_silences_summary(self) -> None:
        proc = self._run_enrich(TARGET_POLICY + "[telemetry]\nconsole = false\n")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn(b"keel \xe2\x96\xb8 wrapped", proc.stderr)  # banner unaffected
        self.assertNotIn(b"report --open", proc.stderr)

    def test_keel_quiet_silences_summary(self) -> None:
        proc = self._run_enrich(TARGET_POLICY, KEEL_QUIET="1")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertNotIn(b"keel \xe2\x96\xb8", proc.stderr)

    def test_fail_open_prints_no_summary(self) -> None:
        # keel._auto swallows a broken policy: one warning line, app unwrapped.
        with TemporaryDirectory() as d:
            (Path(d) / "keel.toml").write_text("this is [not toml", encoding="utf-8")
            proc = _run(
                [sys.executable, "-c", "import keel._auto, sample_targets; print(sample_targets.enrich_a(1))"],
                env=child_env(KEEL_ENABLE="1"),
                cwd=d,
            )
        self.assertEqual(proc.returncode, 0)
        self.assertEqual(proc.stderr.count(b"auto-activation failed"), 1)
        self.assertNotIn(b"report --open", proc.stderr)
