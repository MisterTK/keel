"""subprocess pack tests: in-process ``cmd:`` durable-flow interception (CCR-5,
chunk-8 / issue #27).

Two layers, mirroring ``test_flows.py``'s dual approach (Tier 2 needs the native
core, which CI's no-wheel path lacks):

* offline dispatch-logic tests against a native-shaped ``_FakeFlowBackend``
  (enter/exit/execute + replay-substitution + E030/E032), so matching,
  cwd-inclusive identity, on_busy, check=True, launch failure, and passthrough
  are all exercised without the compiled module;
* a ``@skipUnless(_NATIVE)`` end-to-end that proves REAL replay-skip against
  ``keel_core`` over an on-disk journal (a side-effect sentinel file proves the
  command is not re-run on a re-dispatch of the same identity).
"""

from __future__ import annotations

import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from keel import _runtime
from keel._policy import CmdFlow
from keel.adapters import subprocess_pack
from keel.adapters.subprocess_pack import (
    KeelCmdFlowBusy,
    KeelCmdFlowDead,
    KeelCmdFlowFailed,
)

try:
    import keel_core  # noqa: F401

    _NATIVE = True
except ImportError:
    _NATIVE = False

_MISSING = "keel-no-such-program-xyz"


class _FakeCoreError(Exception):
    """A native ``KeelCoreError`` stand-in: carries ``.code`` (KEEL-E030/E032)
    the way the PyO3 binding's error does."""

    def __init__(self, code: str, message: str) -> None:
        super().__init__(f"{code}: {message}")
        self.code = code


class _FakeFlowBackend:
    """A native-shaped flow backend double. Models enter/exit, execute (live and
    replay-substituting), a configurable ``enter_flow`` error (E030/E032, for a
    fixed number of calls), and the ``persistent`` journal flag."""

    def __init__(
        self,
        *,
        persistent: bool = True,
        replay: bool = False,
        recorded: "dict | None" = None,
        enter_error: "str | None" = None,
        enter_error_times: int = 1_000_000,
    ) -> None:
        self.persistent = persistent
        self._replay = replay
        self._recorded = recorded
        self._enter_error = enter_error
        self._enter_error_times = enter_error_times
        self.entered: list[tuple] = []
        self.exited: list[str] = []
        self.executed = 0
        self.enter_calls = 0

    def enter_flow(self, entrypoint, args_hash, code_hash=None, explicit_key=None, lease_ms=None):
        self.enter_calls += 1
        if self._enter_error is not None and self.enter_calls <= self._enter_error_times:
            raise _FakeCoreError(self._enter_error, "held/dead")
        self.entered.append((entrypoint, args_hash, code_hash))
        status = "completed" if self._replay else "running"
        return {"flow_id": f"{entrypoint}#{args_hash}#", "status": status, "replay": self._replay}

    def execute(self, request, effect):
        self.executed += 1
        if self._replay and self._recorded is not None:
            return self._recorded  # substitute; the effect is NOT invoked
        result = effect(request)
        return {
            "v": 1,
            "result": result.get("status", "ok"),
            "payload": result.get("payload"),
            "from_cache": False,
        }

    def exit_flow(self, status: str) -> None:
        self.exited.append(status)


def _rule(name: str, patterns: list[str], on_busy: str = "skip") -> dict:
    return {name: CmdFlow(name=name, argv_patterns=patterns, on_busy=on_busy)}


def _py(code: str) -> list[str]:
    """A portable argv that actually runs: ``[python, -c, code]``. Matched by a
    ``["*", "-c", "*"]`` rule."""
    return [sys.executable, "-c", code]


class SubprocessBase(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.cwd = Path(self._tmp.name)
        self._orig_cwd = os.getcwd()
        os.chdir(self.cwd)

    def tearDown(self) -> None:
        subprocess_pack.uninstall()
        _runtime.clear_runtime()
        os.chdir(self._orig_cwd)
        self._tmp.cleanup()

    def _arm(self, backend, cmd_flows: dict) -> None:
        _runtime.set_runtime(backend, None)
        _runtime.set_cmd_flows(cmd_flows)
        subprocess_pack.install()


# --- pure match/identity units (no backend) ---------------------------------


class MatchTest(unittest.TestCase):
    def test_positive_negative_and_positional_length(self) -> None:
        rules = subprocess_pack._compile(_rule("cmd:etl", ["etl", "run"]))
        self.assertIsNotNone(subprocess_pack._match(rules, ["etl", "run"]))
        # trailing observed args are unconstrained
        self.assertIsNotNone(subprocess_pack._match(rules, ["etl", "run", "--verbose"]))
        # too short: fewer observed than pattern positions
        self.assertIsNone(subprocess_pack._match(rules, ["etl"]))
        # a listed position differs
        self.assertIsNone(subprocess_pack._match(rules, ["etl", "stop"]))

    def test_wildcard_single_star_per_segment(self) -> None:
        rules = subprocess_pack._compile(_rule("cmd:etl", ["bash", "-c", "deploy *"]))
        self.assertIsNotNone(subprocess_pack._match(rules, ["bash", "-c", "deploy prod"]))
        self.assertIsNone(subprocess_pack._match(rules, ["bash", "-c", "rollback prod"]))

    def test_ruleless_entrypoint_matches_nothing(self) -> None:
        # A cmd: entrypoint with no [flows.match] rule -> empty argv_patterns ->
        # dropped from the compiled set (matches nothing in-process).
        rules = subprocess_pack._compile(_rule("cmd:bare", []))
        self.assertEqual(rules, ())
        self.assertIsNone(subprocess_pack._match(rules, ["anything"]))

    def test_tie_break_most_specific_wins(self) -> None:
        flows = {
            **_rule("cmd:wide", ["*", "-c", "*"]),
            **_rule("cmd:narrow", ["bash", "-c", "*"]),
        }
        rules = subprocess_pack._compile(flows)
        got = subprocess_pack._match(rules, ["bash", "-c", "x"])
        # Fewer wildcards (2 vs 1... narrow has 1 star, wide has 2) => narrow.
        self.assertEqual(got.entrypoint, "cmd:narrow")

    def test_shell_string_and_non_list_argv_are_not_matchable(self) -> None:
        self.assertIsNone(subprocess_pack._str_argv("echo hi"))  # bare string
        self.assertIsNone(subprocess_pack._str_argv(b"echo"))  # bytes string
        self.assertIsNone(subprocess_pack._str_argv(["ok", 7]))  # non-string element
        self.assertEqual(subprocess_pack._str_argv(["a", "b"]), ["a", "b"])


class IdentityTest(SubprocessBase):
    def test_args_hash_includes_cwd(self) -> None:
        argv = ["etl", "run"]
        d1, d2 = self.cwd / "a", self.cwd / "b"
        d1.mkdir()
        d2.mkdir()
        os.chdir(d1)
        h1 = subprocess_pack._args_hash(argv)
        h1_again = subprocess_pack._args_hash(argv)
        os.chdir(d2)
        h2 = subprocess_pack._args_hash(argv)
        self.assertEqual(h1, h1_again, "same argv + same cwd -> same identity")
        self.assertNotEqual(h1, h2, "same argv, different cwd -> different identity")
        self.assertEqual(len(h1), 16, "16 hex chars, matching exec.rs sha16 width")

    def test_two_calls_same_argv_different_cwd_are_two_flows(self) -> None:
        # End-to-end through the pack: identical argv in two dirs => two distinct
        # args_hashes reach enter_flow.
        d1, d2 = self.cwd / "one", self.cwd / "two"
        d1.mkdir()
        d2.mkdir()
        backend = _FakeFlowBackend()
        self._arm(backend, _rule("cmd:x", ["*", "-c", "*"]))
        os.chdir(d1)
        subprocess.run(_py("pass"))
        os.chdir(d2)
        subprocess.run(_py("pass"))
        self.assertEqual(len(backend.entered), 2)
        self.assertNotEqual(backend.entered[0][1], backend.entered[1][1])


# --- dispatch / passthrough --------------------------------------------------


class DispatchTest(SubprocessBase):
    def test_unmatched_call_passes_through_untouched(self) -> None:
        backend = _FakeFlowBackend()
        self._arm(backend, _rule("cmd:etl", ["etl", "run"]))
        result = subprocess.run(_py("import sys; sys.exit(0)"))
        self.assertEqual(result.returncode, 0)
        self.assertEqual(backend.entered, [], "an unmatched call never enters a flow")
        self.assertEqual(backend.executed, 0)

    def test_matched_call_journals_and_returns_real_result(self) -> None:
        backend = _FakeFlowBackend()
        self._arm(backend, _rule("cmd:x", ["*", "-c", "*"]))
        result = subprocess.run(_py("print('hello')"), capture_output=True, text=True)
        self.assertEqual(result.returncode, 0)
        self.assertEqual(result.stdout.strip(), "hello", "the REAL result is returned on a live run")
        self.assertEqual(len(backend.entered), 1)
        self.assertEqual(backend.entered[0][0], "cmd:x")
        self.assertEqual(backend.executed, 1)
        self.assertEqual(backend.exited, ["completed"])

    def test_nonzero_exit_is_a_completed_dispatch_not_a_keel_failure(self) -> None:
        backend = _FakeFlowBackend()
        self._arm(backend, _rule("cmd:x", ["*", "-c", "*"]))
        result = subprocess.run(_py("import sys; sys.exit(3)"))
        self.assertEqual(result.returncode, 3, "nonzero returncode passes through as data")
        self.assertEqual(backend.exited, ["completed"], "a command that ran completes the flow")

    def test_no_journal_backend_runs_unwrapped(self) -> None:
        backend = _FakeFlowBackend(persistent=False)  # native core with no journal
        self._arm(backend, _rule("cmd:x", ["*", "-c", "*"]))
        result = subprocess.run(_py("print('ran')"), capture_output=True, text=True)
        self.assertEqual(result.stdout.strip(), "ran")
        self.assertEqual(backend.entered, [], "no durable dispatch without a journal")

    def test_inside_an_active_flow_runs_unwrapped(self) -> None:
        backend = _FakeFlowBackend()
        self._arm(backend, _rule("cmd:x", ["*", "-c", "*"]))
        _runtime.set_flow_active(True)  # already inside a Tier-2 flow
        try:
            subprocess.run(_py("pass"))
        finally:
            _runtime.set_flow_active(False)
        self.assertEqual(backend.entered, [], "one flow per process: no nested cmd flow")


# --- replay-skip -------------------------------------------------------------


class ReplayTest(SubprocessBase):
    def test_completed_flow_replays_recorded_result_without_rerunning(self) -> None:
        payload = subprocess_pack._payload_run(0, ["x", "-c", "print"], b"cached-out", None, False)
        backend = _FakeFlowBackend(replay=True, recorded={"result": "ok", "payload": payload})
        self._arm(backend, _rule("cmd:x", ["*", "-c", "*"]))
        # A poison command that WOULD fail loudly if actually run — proves the
        # effect never fires on replay-skip.
        result = subprocess.run(_py("import sys; sys.exit(99)"), capture_output=True)
        self.assertEqual(result.returncode, 0, "the recorded (not the live) outcome is returned")
        self.assertEqual(result.stdout, b"cached-out")
        self.assertEqual(backend.executed, 1)
        self.assertEqual(backend.exited, ["completed"])

    def test_replay_of_a_launch_failed_flow_raises_loudly(self) -> None:
        backend = _FakeFlowBackend(replay=True, recorded={"result": "error", "payload": None})
        self._arm(backend, _rule("cmd:x", ["*", "-c", "*"]))
        with self.assertRaises(KeelCmdFlowFailed):
            subprocess.run(_py("pass"))
        self.assertEqual(backend.exited, ["failed"])


# --- on_busy -----------------------------------------------------------------


class OnBusyTest(SubprocessBase):
    def test_fail_raises_immediately(self) -> None:
        backend = _FakeFlowBackend(enter_error="KEEL-E030")
        self._arm(backend, _rule("cmd:x", ["*", "-c", "*"], on_busy="fail"))
        with self.assertRaises(KeelCmdFlowBusy):
            subprocess.run(_py("pass"))
        self.assertEqual(backend.executed, 0, "a fail-busy never runs the command")

    def test_skip_runs_the_real_command_unwrapped(self) -> None:
        backend = _FakeFlowBackend(enter_error="KEEL-E030")
        self._arm(backend, _rule("cmd:x", ["*", "-c", "*"], on_busy="skip"))
        result = subprocess.run(_py("print('skipped-through')"), capture_output=True, text=True)
        self.assertEqual(result.stdout.strip(), "skipped-through", "skip => real unwrapped result")
        self.assertEqual(backend.executed, 0, "skip forfeits durable tracking, never fabricates")

    def test_dead_flow_is_a_hard_failure(self) -> None:
        backend = _FakeFlowBackend(enter_error="KEEL-E032")
        self._arm(backend, _rule("cmd:x", ["*", "-c", "*"], on_busy="skip"))
        with self.assertRaises(KeelCmdFlowDead):
            subprocess.run(_py("pass"))  # E032 is never skipped, regardless of on_busy

    def test_wait_retries_then_succeeds(self) -> None:
        backend = _FakeFlowBackend(enter_error="KEEL-E030", enter_error_times=2)
        self._arm(backend, _rule("cmd:x", ["*", "-c", "*"], on_busy="wait"))
        with mock.patch.object(subprocess_pack, "_POLL_S", 0.001), mock.patch.object(
            subprocess_pack, "_MAX_WAIT_S", 5.0
        ):
            result = subprocess.run(_py("print('waited')"), capture_output=True, text=True)
        self.assertEqual(result.stdout.strip(), "waited")
        self.assertEqual(backend.enter_calls, 3, "two E030s then a success")
        self.assertEqual(backend.exited, ["completed"])

    def test_wait_gives_up_at_the_bounded_ceiling(self) -> None:
        backend = _FakeFlowBackend(enter_error="KEEL-E030")  # never clears
        self._arm(backend, _rule("cmd:x", ["*", "-c", "*"], on_busy="wait"))
        with mock.patch.object(subprocess_pack, "_POLL_S", 0.01), mock.patch.object(
            subprocess_pack, "_MAX_WAIT_S", 0.05
        ):
            with self.assertRaises(KeelCmdFlowBusy):
                subprocess.run(_py("pass"))


# --- check=True / call seam / launch failure ---------------------------------


class CheckAndCallTest(SubprocessBase):
    def test_check_true_nonzero_raises_calledprocesserror_live(self) -> None:
        backend = _FakeFlowBackend()
        self._arm(backend, _rule("cmd:x", ["*", "-c", "*"]))
        with self.assertRaises(subprocess.CalledProcessError) as ctx:
            subprocess.run(_py("import sys; sys.exit(7)"), check=True)
        self.assertEqual(ctx.exception.returncode, 7)
        self.assertEqual(backend.exited, ["completed"], "the command DID run; the flow completes")

    def test_check_true_nonzero_replays_as_calledprocesserror(self) -> None:
        payload = subprocess_pack._payload_run(7, ["x", "-c", "boom"], b"", b"err", True)
        backend = _FakeFlowBackend(replay=True, recorded={"result": "ok", "payload": payload})
        self._arm(backend, _rule("cmd:x", ["*", "-c", "*"]))
        with self.assertRaises(subprocess.CalledProcessError) as ctx:
            subprocess.run(_py("pass"), check=True)  # would-succeed live; replay raises
        self.assertEqual(ctx.exception.returncode, 7, "check raise semantics survive replay")

    def test_call_seam_matched_returns_returncode(self) -> None:
        backend = _FakeFlowBackend()
        self._arm(backend, _rule("cmd:x", ["*", "-c", "*"]))
        rc = subprocess.call(_py("import sys; sys.exit(5)"))
        self.assertEqual(rc, 5, "call returns the bare returncode int")
        self.assertEqual(backend.exited, ["completed"])

    def test_check_call_raises_via_the_call_seam(self) -> None:
        backend = _FakeFlowBackend()
        self._arm(backend, _rule("cmd:x", ["*", "-c", "*"]))
        # check_call routes through the module-global call() (our seam); its own
        # CalledProcessError raise is on the returned int, outside the seam.
        with self.assertRaises(subprocess.CalledProcessError):
            subprocess.check_call(_py("import sys; sys.exit(4)"))
        self.assertEqual(backend.exited, ["completed"], "the call was journaled as completed")

    def test_check_output_is_covered_via_the_run_seam(self) -> None:
        backend = _FakeFlowBackend()
        self._arm(backend, _rule("cmd:x", ["*", "-c", "*"]))
        out = subprocess.check_output(_py("print('captured')"), text=True)
        self.assertEqual(out.strip(), "captured")
        self.assertEqual(len(backend.entered), 1, "check_output dispatches through run")

    def test_launch_failure_propagates_and_marks_failed(self) -> None:
        backend = _FakeFlowBackend()
        self._arm(backend, _rule("cmd:missing", [_MISSING]))
        with self.assertRaises(FileNotFoundError):
            subprocess.run([_MISSING])  # the command never runs
        self.assertEqual(backend.exited, ["failed"], "a launch failure marks the flow failed")


# --- install lifecycle -------------------------------------------------------


class LifecycleTest(unittest.TestCase):
    def test_install_wraps_and_uninstall_restores(self) -> None:
        orig_run, orig_call = subprocess.run, subprocess.call
        subprocess_pack.install()
        try:
            self.assertTrue(getattr(subprocess.run, "__keel_wrapped__", False))
            self.assertTrue(getattr(subprocess.call, "__keel_wrapped__", False))
            subprocess_pack.install()  # double install: no double-wrap
        finally:
            subprocess_pack.uninstall()
        self.assertIs(subprocess.run, orig_run)
        self.assertIs(subprocess.call, orig_call)

    def test_disabled_backend_is_transparent(self) -> None:
        # No runtime set (get_backend() is None): wrapper falls straight through.
        subprocess_pack.install()
        self.addCleanup(subprocess_pack.uninstall)
        _runtime.clear_runtime()
        result = subprocess.run(_py("print('bare')"), capture_output=True, text=True)
        self.assertEqual(result.stdout.strip(), "bare")


# --- native end-to-end (real replay-skip over an on-disk journal) ------------


@unittest.skipUnless(_NATIVE, "keel_core native module not built (maturin develop in crates/keel-py)")
class NativeReplaySkipTest(unittest.TestCase):
    """The at-most-once proof against the REAL core: a completed cmd flow is
    NOT re-run on a re-dispatch of the same identity — a side-effect sentinel
    file is written exactly once across both runs, and the second run's stdout
    is served from the journal."""

    def setUp(self) -> None:
        from keel._backend import load_backend
        from keel._defaults import level0_defaults

        self._load_backend = load_backend
        self._level0 = level0_defaults
        self._tmp = tempfile.TemporaryDirectory()
        self.cwd = Path(self._tmp.name)
        self._orig_cwd = os.getcwd()
        os.chdir(self.cwd)

    def tearDown(self) -> None:
        subprocess_pack.uninstall()
        _runtime.clear_runtime()
        os.chdir(self._orig_cwd)
        self._tmp.cleanup()

    def _arm(self) -> None:
        backend = self._load_backend("native", cwd=str(self.cwd))
        backend.configure(self._level0())
        _runtime.set_runtime(backend, None)
        _runtime.set_cmd_flows(_rule("cmd:etl", ["*", "-c", "*"]))
        subprocess_pack.install()

    def test_completed_command_is_not_rerun_on_redispatch(self) -> None:
        sentinel = self.cwd / "ran.log"
        code = f"open({str(sentinel)!r}, 'a').write('x'); print('OUT')"
        argv = _py(code)

        self._arm()
        r1 = subprocess.run(argv, capture_output=True, text=True)
        self.assertEqual(r1.returncode, 0)
        self.assertEqual(r1.stdout.strip(), "OUT")
        self.assertEqual(sentinel.read_text(), "x", "the command ran once, live")

        # Fresh backend over the SAME on-disk journal, SAME identity -> replay.
        subprocess_pack.uninstall()
        _runtime.clear_runtime()
        self._arm()
        r2 = subprocess.run(argv, capture_output=True, text=True)
        self.assertEqual(r2.returncode, 0)
        self.assertEqual(r2.stdout.strip(), "OUT", "stdout served from the journal")
        self.assertEqual(sentinel.read_text(), "x", "REPLAY-SKIP: the command did not re-run")


if __name__ == "__main__":
    unittest.main()
