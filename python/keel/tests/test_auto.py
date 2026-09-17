"""keel._auto — the KEEL_ENABLE-gated auto-activation shim (spec §4 / WS2).

Runs the gate contract through real child interpreters (mirrors test_run.py's
pattern): importing ``keel._auto`` must behave exactly like ``keel run``'s
in-process bootstrap when enabled, and must be a no-op / never-fatal
otherwise. The `.pth`-in-a-real-venv leg lives in test_pth_wheel.py (Task 2);
these tests import the module directly, which exercises everything except
site's `.pth` processing itself.
"""

from __future__ import annotations

import importlib.util
import os
import subprocess
import sys
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

from . import FIXTURES, child_env
from keel import bootstrap

_IMPORT_AUTO = "import keel._auto; print('APP-RAN')"

FLOW_TARGET = str(FIXTURES / "flow_target.py")
NOOP = str(FIXTURES / "noop_app.py")
HELLO = str(FIXTURES / "hello_app.py")

# The double-activation repro (WS2 final-review Important finding):
# `import keel._auto` performs the FIRST `install_keel()` — exactly what the
# `.pth` shim's gated import line does (real site processing of that line is
# covered separately by test_pth_wheel.py) — then `main_module()` (the same
# entry `python -m keel run <target>` uses) triggers `_run.run_target`'s OWN
# `install_keel()` call, the SECOND install in this same process.
_DOUBLE_ACTIVATE_THEN_RUN = (
    "import os; os.environ['KEEL_ENABLE'] = '1'\n"
    "import keel._auto\n"
    "import sys; sys.argv = ['keel', 'run', {target!r}]\n"
    "from keel._run import main_module\n"
    "main_module()\n"
)

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


class DoubleActivationStateTest(unittest.TestCase):
    """In-process leg: a second `install_keel()` call in the same process (the
    already-installed early return) must hand back the SAME full state the
    first call produced, not a bare `{"enabled": True, "reason": ...}` marker
    — `_run.run_target` indexes `state["backend"]` unconditionally on the
    KEEL_SIM_PLAN/KEEL_RECORD paths and reads `state.get("flow_entrypoints")`
    for flow dispatch, so a dropped key there is a KeyError or a silently
    skipped flow, not just a cosmetic difference."""

    def test_second_install_returns_the_same_cached_state(self) -> None:
        self.addCleanup(bootstrap.uninstall_keel)
        with TemporaryDirectory() as d:
            Path(d, "keel.toml").write_text(
                '[flows]\nentrypoints = ["py:pipeline:main"]\n', encoding="utf-8"
            )
            env = {"KEEL_QUIET": "1"}
            first = bootstrap.install_keel(cwd=d, env=env)
            second = bootstrap.install_keel(cwd=d, env=env)

        self.assertEqual(second["reason"], "already-installed")
        self.assertTrue(first["enabled"])
        self.assertTrue(second["enabled"])
        for key in (
            "backend",
            "discovery",
            "source",
            "function_targets",
            "flow_entrypoints",
            "adapters",
            "mcp",
        ):
            self.assertIn(key, second, f"{key!r} missing from the already-installed state")
        self.assertIs(second["backend"], first["backend"])
        self.assertIs(second["discovery"], first["discovery"])
        self.assertEqual(second["flow_entrypoints"], first["flow_entrypoints"])
        self.assertTrue(second["flow_entrypoints"], "flow entrypoints must survive a second install")

    def test_failed_first_install_retries_cleanly_instead_of_latching(self) -> None:
        # Round 2 (fail-open regression): a broken keel.toml raises KEEL-E001
        # from `load_policy`, BEFORE `_STATE.installed` is ever set — so a
        # second `install_keel()` call must re-parse the policy and raise the
        # SAME loud error again, not an AssertionError/TypeError from a stale
        # "installed" flag with no cached state to return.
        from keel._errors import KeelError

        self.addCleanup(bootstrap.uninstall_keel)
        with TemporaryDirectory() as d:
            Path(d, "keel.toml").write_text("not [valid toml", encoding="utf-8")
            env = {"KEEL_QUIET": "1"}
            with self.assertRaises(KeelError) as first_ctx:
                bootstrap.install_keel(cwd=d, env=env)
            with self.assertRaises(KeelError) as second_ctx:
                bootstrap.install_keel(cwd=d, env=env)
        self.assertEqual(first_ctx.exception.code, "KEEL-E001")
        self.assertEqual(second_ctx.exception.code, "KEEL-E001", "retry must raise the same error, not crash differently")


class DoubleActivationEndToEndTest(unittest.TestCase):
    """Child-process legs of the same regression, through the real
    `keel._auto` → `keel run` chain (see `_DOUBLE_ACTIVATE_THEN_RUN`)."""

    def setUp(self) -> None:
        self._tmp = TemporaryDirectory()
        self.cwd = self._tmp.name

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def write_policy(self, body: str = "") -> None:
        Path(self.cwd, "keel.toml").write_text(body, encoding="utf-8")

    def test_flow_dispatch_survives_double_activation(self) -> None:
        # A lost `flow_entrypoints` state would make `match_flow` see an empty
        # list and silently fall through to a plain script run — no dispatch,
        # no error, "FLOW-RAN" never printed (main() is only called by flow
        # dispatch, never at import time). A preserved state instead reaches
        # `run_as_flow`, which — on the stub backend, which has no Tier 2
        # surface — fails LOUDLY with KEEL-E005. That loud, precise failure is
        # the discriminating observable: it proves dispatch was attempted.
        self.write_policy('[flows]\nentrypoints = ["py:flow_target:main"]\n')
        code = _DOUBLE_ACTIVATE_THEN_RUN.format(target=FLOW_TARGET)
        # Pin the stub backend explicitly: `child_env` pops any inherited
        # KEEL_BACKEND, so under the native-conformance CI leg (or any dev
        # venv with `keel_core` built/installed) `load_backend`'s "auto"
        # default would detect and use the REAL native core, the flow would
        # actually run, and this test's whole premise — that dispatch is
        # observable only via the stub's loud "no Tier 2 surface" refusal —
        # would be gone (rc 0, no KEEL-E005). The assertion is about state
        # preservation across the double `install_keel()` call, not about
        # which backend is present, so pin it.
        proc = _run(code, env=child_env(KEEL_ENABLE="1", KEEL_QUIET="1", KEEL_BACKEND="stub"), cwd=self.cwd)
        self.assertEqual(proc.returncode, 1, proc.stderr)
        self.assertIn(b"KEEL-E005", proc.stderr)
        self.assertIn(b"needs the native core", proc.stderr)
        self.assertNotIn(b"FLOW-RAN", proc.stdout, "flow dispatch must not silently run main()")

    def test_keel_record_does_not_crash_after_double_activation(self) -> None:
        # Before the fix, the already-installed state had no "backend" key,
        # so `_run.run_target`'s `state["backend"] = install_recording(...)`
        # raised a bare KeyError under KEEL_RECORD.
        self.write_policy()
        with TemporaryDirectory() as record_dir:
            record_path = str(Path(record_dir) / "recording.ndjson")
            code = _DOUBLE_ACTIVATE_THEN_RUN.format(target=NOOP)
            proc = _run(
                code,
                env=child_env(KEEL_ENABLE="1", KEEL_RECORD=record_path, KEEL_QUIET="1"),
                cwd=self.cwd,
            )
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertNotIn(b"KeyError", proc.stderr)

    def test_broken_config_e2e_matches_plain_keel_run_failure(self) -> None:
        # Round 2 (fail-open regression): `keel._auto`'s first install fails
        # open on the broken keel.toml (its own "auto-activation failed" line;
        # host survives), leaving `_STATE.installed` False. The SECOND install
        # (`_run.run_target`'s own bootstrap call) then re-parses the SAME
        # broken policy and raises the SAME KEEL-E001 again, so `keel run`
        # exits loudly — identical to what plain `keel run` on a broken config
        # always does, never a crash from a stale "installed" flag with no
        # cached state (AssertionError pre-round-2, TypeError under -O).
        self.write_policy("not [valid toml")
        code = _DOUBLE_ACTIVATE_THEN_RUN.format(target=HELLO)
        proc = _run(code, env=child_env(KEEL_ENABLE="1"), cwd=self.cwd)
        self.assertEqual(proc.returncode, 1, proc.stderr)
        self.assertIn(b"auto-activation failed", proc.stderr, "the .pth-shim's own fail-open line")
        # `_run.py`'s OWN top-level error line (`f"keel ▸ {code}: {message}"`,
        # emitted from its `except BaseException` handler around the SECOND
        # install) — distinct from the substring "KEEL-E001" that already
        # appears (wrapped, inside parens) in the auto-activation-failed line
        # above, so this specifically proves the second install raised its own
        # clean KeelError rather than an uncaught AssertionError/TypeError
        # (which `is_keel_error` would not recognize, so `_run.py` would
        # re-raise it as a bare traceback instead of this loud one-liner).
        self.assertIn(b"keel \xe2\x96\xb8 KEEL-E001:", proc.stderr, "keel run's own loud config error")
        self.assertNotIn(b"AssertionError", proc.stderr)
        self.assertNotIn(b"Traceback", proc.stderr)
        self.assertNotIn(b"stdout-line-1", proc.stdout, "the fixture must never run on a broken config")


_PROBE_INSTALLED = "import keel._auto; from keel import bootstrap; print('INSTALLED', bootstrap._STATE.installed)"


class StrictKeelCwdTest(unittest.TestCase):
    """WS1: KEEL_CWD asserts where policy lives. Pointing it at a directory
    with no keel.toml refuses activation (keel-free, one error line) unless
    KEEL_POLICY=optional."""

    def setUp(self) -> None:
        self._tmp = TemporaryDirectory()
        self.root = Path(self._tmp.name)

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def _keel_lines(self, proc: subprocess.CompletedProcess[bytes]) -> list[str]:
        return [l for l in proc.stderr.decode().splitlines() if l.startswith("keel ▸")]

    def test_missing_policy_at_keel_cwd_refuses_to_activate(self) -> None:
        proc = _run(_PROBE_INSTALLED, env=child_env(KEEL_ENABLE="1", KEEL_CWD=str(self.root)), cwd=str(self.root))
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn(b"INSTALLED False", proc.stdout)
        lines = self._keel_lines(proc)
        self.assertEqual(len(lines), 1, proc.stderr)
        self.assertEqual(
            lines[0],
            f"keel ▸ error: KEEL_CWD={self.root} is set but {self.root / 'keel.toml'} does not exist — "
            "Keel NOT activated; the app continues without keel "
            "(set KEEL_POLICY=optional to run on production defaults instead)",
        )

    def test_refusal_prints_once_even_when_install_is_called_twice(self) -> None:
        code = "import keel._auto; from keel.bootstrap import install_keel; import os; " \
               "install_keel(cwd=os.environ['KEEL_CWD'], env=os.environ, cwd_source='KEEL_CWD'); print('OK')"
        proc = _run(code, env=child_env(KEEL_ENABLE="1", KEEL_CWD=str(self.root)), cwd=str(self.root))
        self.assertIn(b"OK", proc.stdout)
        self.assertEqual(len(self._keel_lines(proc)), 1, proc.stderr)

    def test_activation_row_names_policy_source_and_root(self) -> None:
        (self.root / "keel.toml").write_text("")
        proc = _run("import keel._auto", env=child_env(KEEL_ENABLE="1", KEEL_CWD=str(self.root)), cwd=str(self.root))
        self.assertEqual(proc.returncode, 0, proc.stderr)
        import sqlite3 as _sq
        conn = _sq.connect(self.root / ".keel" / "discovery.db")
        row = conn.execute("SELECT language, policy_source, policy_path, keel_cwd, cwd FROM activations").fetchone()
        self.assertEqual(row, ("python", "keel.toml", str(self.root / "keel.toml"), str(self.root), str(self.root)))

    def test_refused_activation_writes_no_row(self) -> None:
        proc = _run("import keel._auto", env=child_env(KEEL_ENABLE="1", KEEL_CWD=str(self.root)), cwd=str(self.root))
        self.assertFalse((self.root / ".keel").exists())

    def test_the_latch_does_not_answer_a_later_call_about_a_different_root(self) -> None:
        """The refusal latch is a PRINT-once latch, not a process-wide verdict.

        The `.pth` shim refuses an ambient, stale KEEL_CWD; a later
        `install_keel(cwd=<a real project root>)` — the public API, and the
        path `_run.run_target` takes — was never asked about that root and must
        activate normally instead of inheriting the refusal (which `_run.py`
        would turn into SystemExit(2) on a perfectly valid root).
        Node twin: node/keel/test/bootstrap-refusal-latch.test.mjs.
        """
        good = self.root / "good"
        good.mkdir()
        (good / "keel.toml").write_text("")
        code = (
            "import keel._auto; from keel.bootstrap import install_keel; import os; "
            "r = install_keel(cwd=os.environ['GOOD_ROOT'], env={}, cwd_source='cwd'); "
            "print('ENABLED', r['enabled'])"
        )
        proc = _run(
            code,
            env=child_env(KEEL_ENABLE="1", KEEL_CWD=str(self.root), GOOD_ROOT=str(good)),
            cwd=str(self.root),
        )
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn(b"ENABLED True", proc.stdout, proc.stderr)
        # The shim's refusal still printed exactly once, and the good root's
        # own activation line joins it — never a second refusal.
        lines = self._keel_lines(proc)
        self.assertEqual(len(lines), 2, proc.stderr)
        self.assertIn("Keel NOT activated", lines[0])
        self.assertIn(f"with policy {good / 'keel.toml'}", lines[1])

    def test_path_normalization_corpus_matches_the_node_twin(self) -> None:
        """The shared corpus behind Node's `normalizeCwd` (bootstrap.mjs):
        Node reimplements exactly these rows, so if pathlib's behavior ever
        shifts under us this side fails first. Twin:
        node/keel/test/bootstrap-refusal-latch.test.mjs
        `normalizeCwd reproduces pathlib's normalization byte for byte`.
        Note `/a/../b`: pathlib does NOT resolve `..`, so Node must not
        either (which is why `path.normalize` is the wrong tool there)."""
        corpus = [
            ("/app/", "/app"),
            ("/app", "/app"),
            ("/app//x/./y", "/app/x/y"),
            ("//app", "//app"),
            ("///app", "/app"),
            (".", "."),
            ("./foo", "foo"),
            ("/", "/"),
            ("a/b/", "a/b"),
            ("/a/../b", "/a/../b"),
            ("", "."),
            ("/a/./b/", "/a/b"),
        ]
        if os.name == "nt":  # POSIX rows only — the Node twin skips on win32 too
            self.skipTest("POSIX path corpus")
        for raw, expected in corpus:
            with self.subTest(raw=raw):
                self.assertEqual(str(Path(raw)), expected)

    def test_a_trailing_slash_on_keel_cwd_is_normalized_like_pathlib(self) -> None:
        """`ENV KEEL_CWD=/app/` in a Dockerfile is an ordinary input; Python
        echoes it through `str(Path(cwd))`, which strips the trailing
        separator. Node normalizes the same way (bootstrap.mjs `normalizeCwd`)
        so both front ends print the same bytes. Twin:
        node/keel/test/banner-parent-policy.test.mjs."""
        import json as _json

        proc = _run(
            _PROBE_INSTALLED,
            env=child_env(KEEL_ENABLE="1", KEEL_CWD=f"{self.root}/", KEEL_LOG_FORMAT="json"),
            cwd=str(self.root),
        )
        objs = [_json.loads(l) for l in proc.stderr.decode().splitlines() if l.strip()]
        self.assertEqual(len(objs), 1, proc.stderr)
        self.assertEqual(objs[0]["keel_cwd"], str(self.root), "no trailing slash survives")

        text = _run(
            _PROBE_INSTALLED,
            env=child_env(KEEL_ENABLE="1", KEEL_CWD=f"{self.root}/"),
            cwd=str(self.root),
        )
        lines = self._keel_lines(text)
        self.assertEqual(len(lines), 1, text.stderr)
        self.assertEqual(
            lines[0],
            f"keel ▸ error: KEEL_CWD={self.root} is set but {self.root / 'keel.toml'} does not exist — "
            "Keel NOT activated; the app continues without keel "
            "(set KEEL_POLICY=optional to run on production defaults instead)",
        )

    def test_keel_policy_optional_restores_defaults_with_a_warning(self) -> None:
        proc = _run(
            _PROBE_INSTALLED,
            env=child_env(KEEL_ENABLE="1", KEEL_CWD=str(self.root), KEEL_POLICY="optional"),
            cwd=str(self.root),
        )
        self.assertIn(b"INSTALLED True", proc.stdout)
        lines = self._keel_lines(proc)
        self.assertEqual(len(lines), 1, proc.stderr)
        self.assertIn("with production defaults — KEEL_CWD=", lines[0])
        self.assertIn("(KEEL_POLICY=optional)", lines[0])

    def test_keel_cwd_with_a_policy_file_activates_normally(self) -> None:
        (self.root / "keel.toml").write_text("")
        proc = _run(_PROBE_INSTALLED, env=child_env(KEEL_ENABLE="1", KEEL_CWD=str(self.root)), cwd=str(self.root))
        self.assertIn(b"INSTALLED True", proc.stdout)
        self.assertIn(f"with policy {self.root / 'keel.toml'}", proc.stderr.decode())

    def test_python_m_keel_run_exits_2_under_a_stale_keel_cwd(self) -> None:
        proc = _run(
            "import sys; sys.argv=['keel','run',%r]; from keel._run import main_module; main_module()" % HELLO,
            env=child_env(KEEL_CWD=str(self.root)),
            cwd=str(self.root),
        )
        self.assertEqual(proc.returncode, 2, proc.stderr)
        self.assertIn(b"Keel NOT activated", proc.stderr)
        self.assertNotIn(b"hello", proc.stdout, "the target must not run")

    def test_json_log_format_emits_one_object_per_line(self) -> None:
        import json as _json
        # R10: `note` carries the activation line's em-dash tail whenever the
        # text form has one, and the key is ABSENT when it does not. Pin both
        # sides — the defaults run first, before a keel.toml exists anywhere
        # at or above it, so the tail is the plain `keel init` nudge.
        # The child's own getcwd() drops symlink hops (macOS /var → /private/var)
        # before the banner ever renders the path, so compare against the
        # REALPATH, not the tempdir path TemporaryDirectory handed back.
        sub = (self.root / "sub").resolve()
        sub.mkdir()
        defaults = _run(
            "import keel._auto",
            env=child_env(KEEL_ENABLE="1", KEEL_LOG_FORMAT="json"),
            cwd=str(sub),
        )
        activation = _json.loads(defaults.stderr.decode().splitlines()[0])
        self.assertEqual(activation["keel"], "activation", defaults.stderr)
        self.assertEqual(activation["policy_source"], "defaults")
        self.assertEqual(activation["note"], f"no keel.toml in {sub}; `keel init` to customize")

        (self.root / "keel.toml").write_text("")
        proc = _run(
            "import keel._auto; import sample_targets; sample_targets.enrich_a(1)",
            env=child_env(KEEL_ENABLE="1", KEEL_CWD=str(self.root), KEEL_LOG_FORMAT="json"),
            cwd=str(self.root),
        )
        lines = [l for l in proc.stderr.decode().splitlines() if l.strip()]
        objs = [_json.loads(l) for l in lines]
        kinds = [o["keel"] for o in objs]
        self.assertEqual(kinds, ["activation", "summary"], proc.stderr)
        self.assertEqual(objs[0]["policy_source"], "keel.toml")
        self.assertEqual(objs[0]["policy_path"], str(self.root / "keel.toml"))
        self.assertEqual(objs[0]["root_source"], "KEEL_CWD")
        self.assertNotIn("note", objs[0], "a loaded policy has no em-dash tail — no note key")
        self.assertEqual(objs[1]["keel_cwd"], str(self.root))

    def test_backend_is_named_end_to_end_under_the_stub(self) -> None:
        # #119 transparency: KEEL_BACKEND=auto (the default) falls back to the
        # stub silently on a failed native import — the stub has no durable
        # flows and no cross-run cache persistence, so a user must be able to
        # tell which backend they got from the real, wired-up output (not
        # just the `_banner`/`format_summary_json` unit tests).
        (self.root / "keel.toml").write_text("")
        proc = _run(
            "import keel._auto; import sample_targets; sample_targets.enrich_a(1)",
            env=child_env(
                KEEL_ENABLE="1", KEEL_CWD=str(self.root), KEEL_BACKEND="stub", KEEL_LOG_FORMAT="json"
            ),
            cwd=str(self.root),
        )
        import json as _json

        lines = [l for l in proc.stderr.decode().splitlines() if l.strip()]
        objs = [_json.loads(l) for l in lines]
        kinds = [o["keel"] for o in objs]
        self.assertEqual(kinds, ["activation", "summary"], proc.stderr)
        self.assertEqual(objs[1]["backend"], "stub")

        text_proc = _run(
            "import keel._auto",
            env=child_env(KEEL_ENABLE="1", KEEL_CWD=str(self.root), KEEL_BACKEND="stub"),
            cwd=str(self.root),
        )
        self.assertIn(
            "pure-Python backend: no durable flows, no cross-run cache", text_proc.stderr.decode()
        )

    def test_paused_stub_seam_is_named_end_to_end(self) -> None:
        # #121: `KEEL_STUB_PAUSED` reinstates the entire #119 defect (no
        # backoff/throttling/pacing) with zero evidence anywhere else in the
        # output — a real, wired-up process that somehow inherits it must say
        # so, not just the `_banner` unit tests. `child_env` normally strips
        # this var (it is `**extra`, applied AFTER the strip), so this is the
        # one place in the suite that deliberately lets it through.
        (self.root / "keel.toml").write_text("")
        text_proc = _run(
            "import keel._auto",
            env=child_env(
                KEEL_ENABLE="1", KEEL_CWD=str(self.root), KEEL_BACKEND="stub", KEEL_STUB_PAUSED="1"
            ),
            cwd=str(self.root),
        )
        out = text_proc.stderr.decode()
        self.assertIn("pure-Python backend: no durable flows, no cross-run cache", out)
        self.assertIn("KEEL_STUB_PAUSED is set — no pacing", out)

        # …and under KEEL_LOG_FORMAT=json, where `emit` writes the OBJECT
        # INSTEAD of that text: the deployed, structured-logging process is
        # the one this warning exists for, so both backend facts have to be
        # indexable FIELDS or they vanish exactly where they matter (F10, the
        # same reason `dev_cache_off` is a field).
        import json as _json

        json_proc = _run(
            "import keel._auto",
            env=child_env(
                KEEL_ENABLE="1",
                KEEL_CWD=str(self.root),
                KEEL_BACKEND="stub",
                KEEL_STUB_PAUSED="1",
                KEEL_LOG_FORMAT="json",
            ),
            cwd=str(self.root),
        )
        act = _json.loads(json_proc.stderr.decode().splitlines()[0])
        self.assertEqual(act["keel"], "activation", json_proc.stderr)
        self.assertEqual(act["backend"], "stub", act)
        self.assertIs(act["stub_paused"], True, act)
        self.assertNotIn("keel ▸", json_proc.stderr.decode())

        # The native backend makes a stray KEEL_STUB_PAUSED inert, so the
        # field must be ABSENT there rather than reporting a seam that is not
        # in force — the `dev_cache_off: null`-shaped lie, in miniature.
        # (`KEEL_BACKEND=native` is a hard failure without the module, so this
        # half only runs where the native core is actually built — and the
        # `importlib` probe, not the child's exit code, decides, so a child
        # that failed for some OTHER reason cannot silently skip the check.)
        if importlib.util.find_spec("keel_core") is not None:
            native_proc = _run(
                "import keel._auto",
                env=child_env(
                    KEEL_ENABLE="1",
                    KEEL_CWD=str(self.root),
                    KEEL_BACKEND="native",
                    KEEL_STUB_PAUSED="1",
                    KEEL_LOG_FORMAT="json",
                ),
                cwd=str(self.root),
            )
            native_act = _json.loads(native_proc.stderr.decode().splitlines()[0])
            self.assertEqual(native_act["backend"], "native", native_act)
            self.assertNotIn("stub_paused", native_act, native_act)

    def test_json_log_format_refusal_is_an_error_object(self) -> None:
        import json as _json
        proc = _run(
            _PROBE_INSTALLED,
            env=child_env(KEEL_ENABLE="1", KEEL_CWD=str(self.root), KEEL_LOG_FORMAT="json"),
            cwd=str(self.root),
        )
        objs = [_json.loads(l) for l in proc.stderr.decode().splitlines() if l.strip()]
        self.assertEqual(len(objs), 1)
        self.assertEqual(objs[0]["keel"], "error")
        self.assertEqual(objs[0]["code"], "policy-missing-at-keel-cwd")
        self.assertEqual(objs[0]["keel_cwd"], str(self.root))


if __name__ == "__main__":
    unittest.main()
