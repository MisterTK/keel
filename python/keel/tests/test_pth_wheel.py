"""The .pth-in-a-real-venv leg (spec §4's integration requirement): build the
wheel, pip-install it into a scratch venv, and prove (a) KEEL_ENABLE=1
activates through site's .pth processing, (b) without the gate no keel
module is imported at all, (c) a broken policy never kills the host.

Skips cleanly when the `build` package is unavailable (offline CI legs get
this coverage from the packaging job instead). Installs with --no-deps: the
wheel vendors the pure-Python stub backend, so keelrun-core is not needed."""

from __future__ import annotations

import glob
import importlib.util
import os
import subprocess
import sys
import unittest
import venv
from pathlib import Path
from tempfile import TemporaryDirectory

_PKG_DIR = Path(__file__).resolve().parents[1]


@unittest.skipUnless(importlib.util.find_spec("build"), "python -m build unavailable")
class PthWheelTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls._tmp = TemporaryDirectory()
        root = Path(cls._tmp.name)
        dist = root / "dist"
        proc = subprocess.run(
            [sys.executable, "-m", "build", "--wheel", "--outdir", str(dist), str(_PKG_DIR)],
            capture_output=True,
        )
        if proc.returncode != 0:
            raise unittest.SkipTest(f"wheel build failed in this environment: {proc.stderr[-400:]!r}")
        (cls.wheel,) = glob.glob(str(dist / "keelrun-*.whl"))
        cls.venv_dir = root / "venv"
        venv.create(cls.venv_dir, with_pip=True)
        cls.py = str(cls.venv_dir / ("Scripts" if os.name == "nt" else "bin") / "python")
        install = subprocess.run(
            [cls.py, "-m", "pip", "install", "--quiet", "--no-deps", cls.wheel], capture_output=True
        )
        if install.returncode != 0:
            raise unittest.SkipTest(f"pip install failed: {install.stderr[-400:]!r}")

    @classmethod
    def tearDownClass(cls) -> None:
        cls._tmp.cleanup()

    def run_in_venv(self, code: str, *, cwd: str, **env_extra: str) -> subprocess.CompletedProcess[bytes]:
        env = {k: v for k, v in os.environ.items() if k not in ("KEEL_ENABLE", "KEEL_DISABLE", "PYTHONPATH")}
        env.update(env_extra)
        return subprocess.run([self.py, "-c", code], env=env, cwd=cwd, capture_output=True)

    def test_pth_lands_at_site_packages_root(self) -> None:
        proc = self.run_in_venv(
            "import site, os; print(any(os.path.exists(os.path.join(p, 'keelrun_activate.pth')) for p in site.getsitepackages()))",
            cwd=self._tmp.name,
        )
        self.assertIn(b"True", proc.stdout, proc.stderr)

    def test_gate_on_activates_through_real_site_processing(self) -> None:
        with TemporaryDirectory() as cwd:
            Path(cwd, "keel.toml").write_text("", encoding="utf-8")
            proc = self.run_in_venv("print('APP-RAN')", cwd=cwd, KEEL_ENABLE="1")
            self.assertEqual(proc.returncode, 0, proc.stderr)
            self.assertIn(b"APP-RAN", proc.stdout)
            self.assertIn(b"keel \xe2\x96\xb8", proc.stderr, "banner proves the .pth fired")

    def test_gate_off_means_keel_never_imported(self) -> None:
        with TemporaryDirectory() as cwd:
            proc = self.run_in_venv(
                "import sys; print('keel-loaded', any(m == 'keel' or m.startswith('keel.') for m in sys.modules))",
                cwd=cwd,
            )
            self.assertIn(b"keel-loaded False", proc.stdout, "idle install must not import keel")
            self.assertEqual(proc.stderr, b"")

    def test_broken_policy_survivable_under_pth(self) -> None:
        with TemporaryDirectory() as cwd:
            Path(cwd, "keel.toml").write_text("not [valid toml", encoding="utf-8")
            proc = self.run_in_venv("print('APP-RAN')", cwd=cwd, KEEL_ENABLE="1")
            self.assertEqual(proc.returncode, 0)
            self.assertIn(b"APP-RAN", proc.stdout)
            self.assertIn(b"auto-activation failed", proc.stderr)


if __name__ == "__main__":
    unittest.main()
