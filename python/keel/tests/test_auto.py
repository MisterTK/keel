"""keel._auto — the KEEL_ENABLE-gated auto-activation shim (spec §4 / WS2).

Runs the gate contract through real child interpreters (mirrors test_run.py's
pattern): importing ``keel._auto`` must behave exactly like ``keel run``'s
in-process bootstrap when enabled, and must be a no-op / never-fatal
otherwise. The `.pth`-in-a-real-venv leg lives in test_pth_wheel.py (Task 2);
these tests import the module directly, which exercises everything except
site's `.pth` processing itself.
"""

from __future__ import annotations

import subprocess
import sys
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

from . import child_env

_IMPORT_AUTO = "import keel._auto; print('APP-RAN')"

# Discovery (`_discovery.py:196-209`) only creates `.keel/` on the FIRST
# recorded call — construction just sets `db_path`, it does not `mkdir`. A
# bare `import keel._auto` with no target invocation therefore never touches
# the filesystem under cwd, so proving "state roots at cwd/KEEL_CWD" needs a
# real intercepted call, not just an import. This mirrors test_run.py's
# FullPipelineTest: declare a `py:` function target and call it through the
# fixtures package `child_env` already puts on PYTHONPATH.
_TARGET_POLICY = '[target."py:sample_targets.enrich_*"]\n'
_CALL_TARGET = "import keel._auto, sample_targets; print('APP-RAN', sample_targets.enrich_a(41))"


def _run(code: str, *, env: dict[str, str], cwd: str) -> subprocess.CompletedProcess[bytes]:
    return subprocess.run([sys.executable, "-c", code], env=env, cwd=cwd, capture_output=True)


class AutoActivationTest(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = TemporaryDirectory()
        self.cwd = self._tmp.name

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def write_policy(self, body: str = "", subdir: str = "") -> Path:
        root = Path(self.cwd, subdir)
        root.mkdir(parents=True, exist_ok=True)
        path = root / "keel.toml"
        path.write_text(body, encoding="utf-8")
        return path

    def test_enabled_activates_the_real_bootstrap(self) -> None:
        self.write_policy(_TARGET_POLICY)
        proc = _run(_CALL_TARGET, env=child_env(KEEL_ENABLE="1"), cwd=self.cwd)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn(b"APP-RAN", proc.stdout)
        self.assertIn(b"keel \xe2\x96\xb8", proc.stderr, "bootstrap banner emitted")
        self.assertTrue(
            Path(self.cwd, ".keel", "discovery.db").exists(),
            "discovery/journal root created at cwd after a real recorded call",
        )

    def test_unset_gate_means_no_keel_import_at_all(self) -> None:
        self.write_policy()
        # The gate lives in the .pth LINE, not in the module import — so this
        # test asserts the module-level contract instead: _activate() checks
        # the gate again and does nothing (belt and suspenders, and it makes
        # `import keel._auto` safe under any future import path).
        code = "import sys, keel._auto; print('.keel-modules', [m for m in sys.modules if m == 'keel.bootstrap'])"
        proc = _run(code, env=child_env(), cwd=self.cwd)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn(b".keel-modules []", proc.stdout, "bootstrap never imported when gate is off")
        self.assertEqual(proc.stderr, b"", "no banner, no warning")
        self.assertFalse(Path(self.cwd, ".keel").exists())

    def test_gate_is_case_and_whitespace_tolerant(self) -> None:
        self.write_policy()
        proc = _run(_IMPORT_AUTO, env=child_env(KEEL_ENABLE="  TRUE "), cwd=self.cwd)
        self.assertIn(b"keel \xe2\x96\xb8", proc.stderr)

    def test_keel_disable_beats_keel_enable(self) -> None:
        self.write_policy()
        proc = _run(_IMPORT_AUTO, env=child_env(KEEL_ENABLE="1", KEEL_DISABLE="1"), cwd=self.cwd)
        self.assertEqual(proc.returncode, 0)
        self.assertEqual(proc.stderr, b"", "disabled: silent no-op")
        self.assertFalse(Path(self.cwd, ".keel").exists())

    def test_broken_policy_warns_once_and_never_kills_the_host(self) -> None:
        self.write_policy("this is [not toml")
        proc = _run(_IMPORT_AUTO, env=child_env(KEEL_ENABLE="1"), cwd=self.cwd)
        self.assertEqual(proc.returncode, 0, "host app must survive a broken keel.toml")
        self.assertIn(b"APP-RAN", proc.stdout)
        self.assertEqual(proc.stderr.count(b"keel \xe2\x96\xb8 auto-activation failed"), 1)

    def test_keel_cwd_relocates_the_activation_root(self) -> None:
        self.write_policy(_TARGET_POLICY, subdir="app")
        proc = _run(_CALL_TARGET, env=child_env(KEEL_ENABLE="1", KEEL_CWD="app"), cwd=self.cwd)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn(b"keel \xe2\x96\xb8", proc.stderr)
        self.assertTrue(
            Path(self.cwd, "app", ".keel", "discovery.db").exists(),
            "journal/discovery rooted at KEEL_CWD after a real recorded call",
        )
        self.assertFalse(Path(self.cwd, ".keel").exists())

    def test_pth_file_content_is_one_gated_import_line(self) -> None:
        pth = Path(__file__).resolve().parents[1] / "keelrun_activate.pth"
        lines = [l for l in pth.read_text(encoding="utf-8").splitlines() if l.strip()]
        self.assertEqual(len(lines), 1, ".pth shims must be a single line")
        line = lines[0]
        self.assertTrue(line.startswith("import "), "site only executes lines starting with 'import'")
        self.assertIn("KEEL_ENABLE", line)
        self.assertIn("keel._auto", line)
        # The gate must run BEFORE any keel import: 'keel' may appear only
        # inside the __import__ call that the gate guards.
        self.assertLess(line.index("KEEL_ENABLE"), line.index("keel._auto"))


if __name__ == "__main__":
    unittest.main()
