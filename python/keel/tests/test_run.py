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

try:  # the native leg of the startup budget only runs when the wheel is built
    import keel_core  # noqa: F401

    _NATIVE = True
except ImportError:
    _NATIVE = False

HELLO = str(FIXTURES / "hello_app.py")
ECHO = str(FIXTURES / "echo_argv.py")
ENRICH = str(FIXTURES / "enrich_app.py")
NOOP = str(FIXTURES / "noop_app.py")
SUBDIR_APP = str(FIXTURES / "subdir_app" / "app.py")
SPAWN_PROBE = str(FIXTURES / "spawn_probe.py")


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
        # The keelrun-py-run entry runs a script directly (no `run` subcommand)
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


class ChildActivationEnvTest(unittest.TestCase):
    """#63: a `keel run`-wrapped script's own subprocess children see
    KEEL_ENABLE/KEEL_CWD so they self-activate via the wheel's `.pth`,
    unless disabled or already set by the user."""

    def test_children_inherit_keel_enable_and_cwd(self) -> None:
        # WS1: KEEL_CWD is only ever exported to children when a policy file
        # actually lives at the activation root (otherwise an exported
        # KEEL_CWD would make a child that self-activates via the .pth
        # wrongly refuse — the exact bug this program fixes) — so this needs
        # a real keel.toml to observe the inherited KEEL_CWD at all.
        with TemporaryDirectory() as d:
            (Path(d) / "keel.toml").write_text("", encoding="utf-8")
            out = subprocess.run(
                [sys.executable, "-m", "keel", "run", SPAWN_PROBE],
                capture_output=True,
                text=True,
                env=child_env(),
                cwd=d,
                check=True,
            )
        enable, _, cwd = out.stdout.strip().partition("|")
        self.assertEqual(enable, "1")
        self.assertTrue(cwd)  # points at the activation root

    def test_defaults_run_exports_enable_but_not_a_policy_less_cwd(self) -> None:
        # WS1 negative case: a plain defaults run (no keel.toml anywhere, no
        # ambient KEEL_CWD) must still export KEEL_ENABLE to children (so
        # they self-activate) but must NOT export KEEL_CWD — exporting a
        # policy-less root would make a child that self-activates via the
        # .pth wrongly refuse under the new strict-KEEL_CWD rule, silently
        # reintroducing the cascading-refusal bug this task exists to close.
        with TemporaryDirectory() as d:
            out = subprocess.run(
                [sys.executable, "-m", "keel", "run", SPAWN_PROBE],
                capture_output=True,
                text=True,
                env=child_env(),
                cwd=d,
                check=True,
            )
        enable, _, cwd = out.stdout.strip().partition("|")
        self.assertEqual(enable, "1")
        self.assertEqual(cwd, "", "no keel.toml at the run root: KEEL_CWD must not be exported")

    def test_disabled_run_exports_nothing(self) -> None:
        with TemporaryDirectory() as d:
            out = subprocess.run(
                [sys.executable, "-m", "keel", "run", SPAWN_PROBE],
                capture_output=True,
                text=True,
                env=child_env(KEEL_DISABLE="1"),
                cwd=d,
                check=True,
            )
        self.assertEqual(out.stdout.strip(), "|")

    def test_user_set_keel_enable_is_not_stomped(self) -> None:
        with TemporaryDirectory() as d:
            out = subprocess.run(
                [sys.executable, "-m", "keel", "run", SPAWN_PROBE],
                capture_output=True,
                text=True,
                env=child_env(KEEL_ENABLE="yes"),
                cwd=d,
                check=True,
            )
        self.assertEqual(out.stdout.strip().split("|")[0], "yes")


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
            proc = subprocess.run(cmd, env=env, cwd=cwd, capture_output=True)
            # A failed child (e.g. KEEL_BACKEND=native with no wheel → KEEL-E040)
            # aborts fast and would make the budget assert vacuously — fail loudly.
            if proc.returncode != 0:
                raise AssertionError(
                    f"startup-budget child exited {proc.returncode} for {cmd!r}: "
                    f"{proc.stderr.decode(errors='replace')[:400]}"
                )
            best = min(best, (time.perf_counter() - start) * 1000)
        return best

    def test_keel_run_startup_overhead_under_budget(self) -> None:
        cmd = [sys.executable, "-m", "keel", "run", NOOP]
        with TemporaryDirectory() as d:
            baseline = self._min_ms([sys.executable, NOOP], child_env(), d)
            # Measure both backends explicitly (Task 14 item 5): native is the
            # real core + journal attach; stub is the pure-Python path. `keel run`
            # via child_env auto-selects native when the wheel is installed.
            stub_ms = self._min_ms(cmd, child_env(KEEL_QUIET="1", KEEL_BACKEND="stub"), d)
            # The native leg is measured only when the wheel is built; the
            # no-wheel CI front-end job forces KEEL_BACKEND=native → KEEL-E040,
            # which _min_ms would (correctly) treat as a hard failure, so gate it.
            native_added = None
            if _NATIVE:
                native_ms = self._min_ms(cmd, child_env(KEEL_QUIET="1", KEEL_BACKEND="native"), d)
                native_added = native_ms - baseline
        stub_added = stub_ms - baseline
        native_str = (
            f"native +{native_added:.1f} ms"
            if native_added is not None
            else "native (skipped: no wheel)"
        )
        print(
            f"[startup budget] baseline {baseline:.1f} ms | "
            f"stub +{stub_added:.1f} ms | {native_str} "
            "(target <100 ms, budget <250 ms)",
            file=sys.stderr,
        )
        # Both measured paths must exit 0 (enforced in _min_ms) AND stay under
        # budget — native is the shipped path, stub is the no-wheel CI path.
        self.assertLess(stub_added, 250.0, f"stub startup budget exceeded: {stub_added:.1f} ms")
        if native_added is not None:
            self.assertLess(
                native_added, 250.0, f"native startup budget exceeded: {native_added:.1f} ms"
            )


class BannerTest(unittest.TestCase):
    """The happy-path banner is a single line in the dx-spec format, and never
    reports the awkward 'wrapped 0 call sites' at Level 0 (adapters only)."""

    def _banner(
        self,
        source: str,
        target_keys: list[str],
        adapters: list,
        *,
        env: dict | None = None,
        cwd: str | Path | None = None,
        cwd_source: str = "cwd",
    ) -> str:
        import contextlib
        import io

        from keel.bootstrap import _banner

        buf = io.StringIO()
        with contextlib.redirect_stderr(buf):
            _banner(env if env is not None else {}, source, target_keys, adapters, None, cwd, cwd_source)
        return buf.getvalue()

    def test_level0_banner_lists_adapters_not_zero_call_sites(self) -> None:
        from keel.adapters import Detection

        out = self._banner("defaults", [], [Detection(matched=True, name="httpx", version="0.28")])
        self.assertEqual(out.count("\n"), 1, "banner must be a single line")
        self.assertIn("keel ▸ wrapped httpx 0.28 with production defaults", out)
        self.assertNotIn("0 call site", out)

    def test_banner_with_function_targets_counts_call_sites(self) -> None:
        out = self._banner("keel.toml", ["py:m.enrich"], [])
        self.assertIn("keel ▸ wrapped 1 call site (py:m.enrich) with policy keel.toml", out)

    def test_defaults_banner_names_the_root_it_searched(self) -> None:
        # WS1: the defaults line must say WHERE it looked — "production
        # defaults" alone read as success in the field (F0).
        with TemporaryDirectory() as tmp:
            sub = Path(tmp) / "sub"
            sub.mkdir()
            out = self._banner("defaults", [], [], cwd=sub)
        self.assertEqual(
            out,
            f"keel ▸ wrapped nothing yet with production defaults — no keel.toml in {sub}; "
            "`keel init` to customize\n",
        )

    def test_policy_banner_names_the_file(self) -> None:
        with TemporaryDirectory() as tmp:
            out = self._banner("keel.toml", ["py:m.enrich"], [], cwd=Path(tmp))
        self.assertIn(f"with policy {Path(tmp) / 'keel.toml'}", out)

    def test_optional_defaults_under_keel_cwd_warn_in_the_banner(self) -> None:
        with TemporaryDirectory() as tmp:
            root = Path(tmp)
            out = self._banner(
                "defaults", [], [], env={"KEEL_POLICY": "optional"}, cwd=root, cwd_source="KEEL_CWD"
            )
        self.assertEqual(
            out,
            f"keel ▸ wrapped nothing yet with production defaults — KEEL_CWD={root} is set but "
            f"{root / 'keel.toml'} does not exist (KEEL_POLICY=optional)\n",
        )

    def test_defaults_banner_without_parent_keel_toml_is_byte_unchanged(self) -> None:
        # #85: pin today's exact line when no parent keel.toml exists, even
        # though a cwd is now threaded through — nothing should change here.
        with TemporaryDirectory() as tmp:
            sub = Path(tmp) / "sub"
            sub.mkdir()
            out = self._banner("defaults", [], [], cwd=sub)
        self.assertEqual(
            out,
            f"keel ▸ wrapped nothing yet with production defaults — no keel.toml in {sub}; "
            "`keel init` to customize\n",
        )

    def test_defaults_banner_names_parent_keel_toml_and_keel_cwd_fix(self) -> None:
        # #85: a server launched with cwd in a subdirectory of the real
        # project root must not read "production defaults" as normal — the
        # banner must name both paths and the KEEL_CWD fix.
        with TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "keel.toml").write_text("")
            sub = root / "agents" / "worker"
            sub.mkdir(parents=True)
            out = self._banner("defaults", [], [], cwd=sub)
        self.assertEqual(out.count("\n"), 1, "banner must stay a single line")
        self.assertIn(f"found keel.toml at {root}", out)
        self.assertIn(f"running from {sub}", out)
        self.assertIn(f"set KEEL_CWD={root} to load it", out)
        self.assertNotIn("`keel init` to customize", out)

    def test_defaults_banner_parent_keel_toml_beyond_eight_levels_is_not_found(self) -> None:
        # The walk is bounded (8 parent levels) — a keel.toml further up than
        # that must not surface (matches the Node/doctor walk convention).
        with TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "keel.toml").write_text("")
            deep = root
            for i in range(9):
                deep = deep / f"lvl{i}"
            deep.mkdir(parents=True)
            out = self._banner("defaults", [], [], cwd=deep)
        self.assertIn("`keel init` to customize", out)
        self.assertNotIn("found keel.toml", out)

    def test_defaults_banner_parent_keel_toml_quiet_stays_silent(self) -> None:
        with TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "keel.toml").write_text("")
            sub = root / "sub"
            sub.mkdir()
            out = self._banner("defaults", [], [], env={"KEEL_QUIET": "1"}, cwd=sub)
        self.assertEqual(out, "")

    def test_banner_notes_when_a_serverless_marker_turned_the_dev_cache_off(self) -> None:
        with TemporaryDirectory() as tmp:
            out = self._banner("defaults", [], [], env={"K_SERVICE": "render"}, cwd=Path(tmp))
        self.assertIn("with production defaults (dev cache off: K_SERVICE detected) — no keel.toml in", out)

    def test_banner_has_no_dev_cache_note_when_keel_env_is_explicit(self) -> None:
        with TemporaryDirectory() as tmp:
            out = self._banner("defaults", [], [], env={"K_SERVICE": "x", "KEEL_ENV": "prod"}, cwd=Path(tmp))
        self.assertNotIn("dev cache off", out)


if __name__ == "__main__":
    unittest.main()
