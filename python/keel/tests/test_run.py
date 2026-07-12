"""The `keel run` runner, end to end in child processes: KEEL_DISABLE
byte-identity, the startup banner + clean stdout, exit-code and argv
passthrough, the full import-hook pipeline, and the startup budget."""

from __future__ import annotations

import subprocess
import sys
import time
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

from . import FIXTURES, child_env

HELLO = str(FIXTURES / "hello_app.py")
ECHO = str(FIXTURES / "echo_argv.py")
ENRICH = str(FIXTURES / "enrich_app.py")
NOOP = str(FIXTURES / "noop_app.py")
SUBDIR_APP = str(FIXTURES / "subdir_app" / "app.py")


def _run(cmd: list[str], *, env: dict[str, str], cwd: str) -> subprocess.CompletedProcess[bytes]:
    return subprocess.run(cmd, env=env, cwd=cwd, capture_output=True)


class DisableIdentityTest(unittest.TestCase):
    """DX invariant: KEEL_DISABLE=1 makes a run byte-identical to one with no
    Keel at all — same stdout, same stderr, same exit code."""

    def test_disabled_is_byte_identical_to_plain_python(self) -> None:
        with TemporaryDirectory() as d:
            baseline = _run([sys.executable, HELLO], env=child_env(), cwd=d)
            disabled = _run(
                [sys.executable, "-m", "keel", "run", HELLO],
                env=child_env(KEEL_DISABLE="1"),
                cwd=d,
            )
        self.assertEqual(disabled.returncode, baseline.returncode)
        self.assertEqual(disabled.stdout, baseline.stdout)
        self.assertEqual(disabled.stderr, baseline.stderr)
        self.assertEqual(baseline.returncode, 7)
        # No `.keel` written under a disabled run.
        self.assertFalse((Path(d) / ".keel").exists())

    def test_sibling_import_byte_identical_for_script_in_subdirectory(self) -> None:
        # `keel run subdir/app.py` must put the script's directory on sys.path
        # exactly like `python subdir/app.py`, so a sibling `import helpers`
        # resolves identically. Proven byte-for-byte for the disabled run.
        with TemporaryDirectory() as d:
            baseline = _run([sys.executable, SUBDIR_APP], env=child_env(), cwd=d)
            disabled = _run(
                [sys.executable, "-m", "keel", "run", SUBDIR_APP],
                env=child_env(KEEL_DISABLE="1"),
                cwd=d,
            )
        self.assertEqual(baseline.returncode, 5, baseline.stderr.decode())
        self.assertEqual(baseline.stdout, b"helper says 99\n")
        self.assertEqual(disabled.returncode, baseline.returncode)
        self.assertEqual(disabled.stdout, baseline.stdout)
        self.assertEqual(disabled.stderr, baseline.stderr)

    def test_sibling_import_resolves_under_enabled_keel_run(self) -> None:
        with TemporaryDirectory() as d:
            enabled = _run(
                [sys.executable, "-m", "keel", "run", SUBDIR_APP],
                env=child_env(KEEL_QUIET="1"),
                cwd=d,
            )
        self.assertEqual(enabled.returncode, 5, enabled.stderr.decode())
        self.assertEqual(enabled.stdout, b"helper says 99\n")


class BannerAndPassthroughTest(unittest.TestCase):
    def test_banner_on_stderr_stdout_stays_clean(self) -> None:
        with TemporaryDirectory() as d:
            enabled = _run([sys.executable, "-m", "keel", "run", HELLO], env=child_env(), cwd=d)
        self.assertEqual(enabled.returncode, 7)
        # stdout is exactly the program's output — no Keel noise.
        self.assertEqual(enabled.stdout, b"stdout-line-1\ncomputed 42\n")
        # banner + the program's own stderr both land on stderr.
        self.assertIn("keel ▸ wrapped", enabled.stderr.decode())
        self.assertIn("stderr-line-1", enabled.stderr.decode())

    def test_argv_passthrough_matches_plain_python(self) -> None:
        with TemporaryDirectory() as d:
            baseline = _run([sys.executable, ECHO, "a", "b"], env=child_env(), cwd=d)
            enabled = _run(
                [sys.executable, "-m", "keel", "run", ECHO, "a", "b"],
                env=child_env(KEEL_QUIET="1"),  # silence banner so stdout is comparable
                cwd=d,
            )
        self.assertEqual(enabled.stdout, baseline.stdout)
        self.assertEqual(enabled.stdout.decode().strip(), f"[{ECHO!r}, 'a', 'b']")

    def test_run_entry_exit_code_passthrough(self) -> None:
        # The keel-py-run entry runs a script directly (no `run` subcommand)
        # and passes the script's exit code through.
        with TemporaryDirectory() as d:
            proc = _run(
                [sys.executable, "-c", "import sys; from keel._run import main_run_entry; main_run_entry()", HELLO],
                env=child_env(KEEL_QUIET="1"),
                cwd=d,
            )
        self.assertEqual(proc.returncode, 7)

    def test_missing_target_is_usage_error(self) -> None:
        with TemporaryDirectory() as d:
            proc = _run([sys.executable, "-m", "keel"], env=child_env(), cwd=d)
        self.assertEqual(proc.returncode, 2)
        self.assertIn("usage:", proc.stderr.decode())


class FullPipelineTest(unittest.TestCase):
    def test_keel_run_wraps_and_records_discovery(self) -> None:
        toml = (
            '[target."py:sample_targets.enrich_*"]\n'
            'retry = { attempts = 3, on = ["other"], schedule = "fixed(1ms)" }\n'
        )
        with TemporaryDirectory() as d:
            (Path(d) / "keel.toml").write_text(toml, encoding="utf-8")
            proc = _run([sys.executable, "-m", "keel", "run", ENRICH], env=child_env(), cwd=d)
            self.assertEqual(proc.returncode, 0, proc.stderr.decode())
            self.assertEqual(proc.stdout, b"enriched 42\n")
            self.assertIn("wrapped 1 call site", proc.stderr.decode())
            self.assertIn("py:sample_targets.enrich_*", proc.stderr.decode())

            db = Path(d) / ".keel" / "discovery.db"
            self.assertTrue(db.exists(), "discovery.db must be written")
            import sqlite3

            conn = sqlite3.connect(db)
            conn.row_factory = sqlite3.Row
            try:
                rows = {r["target"]: r for r in conn.execute("SELECT * FROM discovery")}
            finally:
                conn.close()
            row = rows["py:sample_targets.enrich_*"]
            self.assertEqual(row["calls"], 1)
            self.assertEqual(row["successes"], 1)


class StartupBudgetTest(unittest.TestCase):
    """DX invariant 8: `keel run` adds <100ms to process startup at p50. We
    measure the real wall-clock delta between `keel run noop` and plain
    `python noop` (min of a few runs, the most stable estimator of fixed
    overhead), and assert generously to avoid CI flake."""

    @staticmethod
    def _min_ms(cmd: list[str], env: dict[str, str], cwd: str, runs: int = 5) -> float:
        best = float("inf")
        for _ in range(runs):
            start = time.perf_counter()
            subprocess.run(cmd, env=env, cwd=cwd, capture_output=True)
            best = min(best, (time.perf_counter() - start) * 1000)
        return best

    def test_keel_run_startup_overhead_under_budget(self) -> None:
        with TemporaryDirectory() as d:
            baseline = self._min_ms([sys.executable, NOOP], child_env(), d)
            keeled = self._min_ms(
                [sys.executable, "-m", "keel", "run", NOOP], child_env(KEEL_QUIET="1"), d
            )
        added_ms = keeled - baseline
        print(
            f"[startup budget] keel run added {added_ms:.1f} ms "
            f"(baseline {baseline:.1f} ms, keeled {keeled:.1f} ms)",
            file=sys.stderr,
        )
        self.assertLess(added_ms, 250.0, f"startup budget exceeded: {added_ms:.1f} ms")


if __name__ == "__main__":
    unittest.main()
