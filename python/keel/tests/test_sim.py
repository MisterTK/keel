"""Unit tests for `keel._sim` (docs/sim-format.md): the SimBackend fault
injector, its cursor persistence, and directive resolution — all against a
fake `Backend`, no subprocess/network needed. The real end-to-end path
(through `keel run`, a `py:` target's real retry loop reacting to an
injected fault, plus a genuine `SIGKILL` crash-restart) is exercised by
`crates/keel-cli/src/sim.rs`'s `front_end_fault_injection_drives_real_retries`
and `crash_restart_resumes_a_real_tier_2_flow` integration tests."""

from __future__ import annotations

import asyncio
import json
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

from keel._sim import SIM_CRASH_EXIT_CODE, SimBackend, _Cursor, _directive_at, install_sim


class FakeBackend:
    """A minimal `Backend`: `execute`/`execute_async` call `effect(1)` once
    and return whatever it produced, unchanged — so a test can tell exactly
    what `SimBackend` did to the effect it was handed."""

    def __init__(self) -> None:
        self.configured = None

    def configure(self, policy):
        self.configured = policy

    def execute(self, request, effect):
        return effect(1)

    async def execute_async(self, request, effect):
        return await effect(1)

    def report(self):
        return {"reported": True}


def cursor(tmp: Path, name: str = "plan.json") -> _Cursor:
    return _Cursor(tmp / f"{name}.cursor.json")


class DirectiveAtTest(unittest.TestCase):
    def test_selects_by_index(self) -> None:
        directives = [{"kind": "ok"}, {"kind": "crash"}]
        self.assertEqual(_directive_at(directives, 0), {"kind": "ok"})
        self.assertEqual(_directive_at(directives, 1), {"kind": "crash"})
        self.assertIsNone(_directive_at(directives, 2))

    def test_repeat_extends_a_directives_span(self) -> None:
        directives = [{"kind": "timeout", "repeat": 2}, {"kind": "ok"}]
        self.assertEqual(_directive_at(directives, 0)["kind"], "timeout")
        self.assertEqual(_directive_at(directives, 1)["kind"], "timeout")
        self.assertEqual(_directive_at(directives, 2)["kind"], "ok")
        self.assertIsNone(_directive_at(directives, 3))


class CursorPersistenceTest(unittest.TestCase):
    def test_bump_persists_across_instances(self) -> None:
        with TemporaryDirectory() as d:
            path = Path(d, "plan.json.cursor.json")
            c1 = _Cursor(path)
            self.assertEqual(c1.next_index("t"), 0)
            c1.bump("t")
            c1.bump("t")
            # A brand-new instance reading the SAME sidecar sees the bumps —
            # this is what makes a crash-restart continue the sequence.
            c2 = _Cursor(path)
            self.assertEqual(c2.next_index("t"), 2)

    def test_missing_or_corrupt_sidecar_starts_at_zero(self) -> None:
        with TemporaryDirectory() as d:
            path = Path(d, "does-not-exist.cursor.json")
            self.assertEqual(_Cursor(path).next_index("t"), 0)
            corrupt = Path(d, "corrupt.cursor.json")
            corrupt.write_text("not json")
            self.assertEqual(_Cursor(corrupt).next_index("t"), 0)


class SimBackendSyncTest(unittest.TestCase):
    def test_untargeted_call_passes_through_untouched(self) -> None:
        with TemporaryDirectory() as d:
            inner = FakeBackend()
            backend = SimBackend(inner, {"other.target": [{"kind": "crash"}]}, cursor(Path(d)))
            outcome = backend.execute({"target": "t"}, lambda a: {"status": "ok", "payload": a})
            self.assertEqual(outcome, {"status": "ok", "payload": 1})

    def test_ok_directive_still_calls_the_real_effect(self) -> None:
        with TemporaryDirectory() as d:
            inner = FakeBackend()
            backend = SimBackend(inner, {"t": [{"kind": "ok"}]}, cursor(Path(d)))
            calls = []
            outcome = backend.execute({"target": "t"}, lambda a: calls.append(a) or {"status": "ok"})
            self.assertEqual(calls, [1])
            self.assertEqual(outcome, {"status": "ok"})

    def test_conn_and_timeout_and_http_directives_synthesize_the_adapter_shape(self) -> None:
        with TemporaryDirectory() as d:
            cur = cursor(Path(d))
            faults = {"t": [{"kind": "conn"}, {"kind": "timeout"}, {"kind": "5xx"}, {"kind": "429"}]}

            def never(_a):
                raise AssertionError("the real effect must never run for an injected fault")

            backend = SimBackend(FakeBackend(), faults, cur)
            self.assertEqual(backend.execute({"target": "t"}, never)["class"], "conn")
            self.assertEqual(backend.execute({"target": "t"}, never)["class"], "timeout")
            http5xx = backend.execute({"target": "t"}, never)
            self.assertEqual(http5xx["class"], "http")
            self.assertEqual(http5xx["http_status"], 503)
            http429 = backend.execute({"target": "t"}, never)
            self.assertEqual(http429["http_status"], 429)

    def test_explicit_status_and_retry_after_are_honored(self) -> None:
        with TemporaryDirectory() as d:
            backend = SimBackend(
                FakeBackend(),
                {"t": [{"kind": "http", "status": 502, "retry_after_ms": 250}]},
                cursor(Path(d)),
            )
            outcome = backend.execute({"target": "t"}, lambda a: {"status": "ok"})
            self.assertEqual(outcome["http_status"], 502)
            self.assertEqual(outcome["retry_after_ms"], 250)

    def test_spent_sequence_passes_through_live(self) -> None:
        with TemporaryDirectory() as d:
            # `crash` is a no-op here (never actually terminates the process),
            # so `_apply`'s post-crash `raise AssertionError("unreachable")`
            # DOES run — that's the test's signal that the single directive
            # was consumed (a real crash never reaches that line).
            backend = SimBackend(FakeBackend(), {"t": [{"kind": "crash"}]}, cursor(Path(d)), crash=lambda: None)
            with self.assertRaises(AssertionError):
                backend.execute({"target": "t"}, lambda a: {"status": "ok"})
            # The sequence (one directive) is now spent — every further call
            # to this target passes through to the real effect live.
            outcome = backend.execute({"target": "t"}, lambda a: {"status": "ok", "payload": "live"})
            self.assertEqual(outcome, {"status": "ok", "payload": "live"})

    def test_crash_directive_invokes_the_injected_crash_and_never_the_real_effect(self) -> None:
        with TemporaryDirectory() as d:
            crashed = []
            backend = SimBackend(
                FakeBackend(), {"t": [{"kind": "crash"}]}, cursor(Path(d)), crash=lambda: crashed.append(True)
            )
            with self.assertRaises(AssertionError):
                backend.execute({"target": "t"}, lambda a: (_ for _ in ()).throw(AssertionError("must not run")))
            self.assertEqual(crashed, [True])

    def test_cursor_advances_across_repeated_calls_to_the_same_target(self) -> None:
        with TemporaryDirectory() as d:
            backend = SimBackend(
                FakeBackend(), {"t": [{"kind": "timeout"}, {"kind": "ok"}]}, cursor(Path(d))
            )
            first = backend.execute({"target": "t"}, lambda a: {"status": "ok", "payload": "live-1"})
            second = backend.execute({"target": "t"}, lambda a: {"status": "ok", "payload": "live-2"})
            self.assertEqual(first["class"], "timeout")
            self.assertEqual(second, {"status": "ok", "payload": "live-2"})

    def test_configure_and_report_delegate(self) -> None:
        with TemporaryDirectory() as d:
            inner = FakeBackend()
            backend = SimBackend(inner, {}, cursor(Path(d)))
            backend.configure({"x": 1})
            self.assertEqual(inner.configured, {"x": 1})
            self.assertEqual(backend.report(), {"reported": True})


class SimBackendAsyncTest(unittest.TestCase):
    def test_untargeted_and_injected_paths_over_execute_async(self) -> None:
        with TemporaryDirectory() as d:
            inner = FakeBackend()
            backend = SimBackend(inner, {"t": [{"kind": "timeout"}]}, cursor(Path(d)))

            async def scenario():
                passthrough = await backend.execute_async(
                    {"target": "other"}, lambda a: asyncio.sleep(0, result={"status": "ok"})
                )
                injected = await backend.execute_async(
                    {"target": "t"},
                    lambda a: asyncio.sleep(0, result={"status": "ok", "payload": "unreachable"}),
                )
                return passthrough, injected

            passthrough, injected = asyncio.run(scenario())
            self.assertEqual(passthrough, {"status": "ok"})
            self.assertEqual(injected["class"], "timeout")

    def test_async_crash_directive_still_invokes_crash(self) -> None:
        with TemporaryDirectory() as d:
            crashed = []
            backend = SimBackend(
                FakeBackend(), {"t": [{"kind": "crash"}]}, cursor(Path(d)), crash=lambda: crashed.append(True)
            )

            async def scenario():
                await backend.execute_async({"target": "t"}, lambda a: asyncio.sleep(0, result={"status": "ok"}))

            with self.assertRaises(AssertionError):
                asyncio.run(scenario())
            self.assertEqual(crashed, [True])


class InstallSimTest(unittest.TestCase):
    def test_loads_faults_and_wraps_the_backend(self) -> None:
        with TemporaryDirectory() as d:
            plan_path = Path(d, "plan.json")
            plan_path.write_text(
                json.dumps({"v": 1, "target": "x.py", "faults": {"t": [{"kind": "crash"}]}})
            )
            backend = install_sim(FakeBackend(), plan_path=str(plan_path), env={"KEEL_QUIET": "1"})
            self.assertIsInstance(backend, SimBackend)

    def test_missing_faults_block_is_an_empty_map(self) -> None:
        with TemporaryDirectory() as d:
            plan_path = Path(d, "plan.json")
            plan_path.write_text(json.dumps({"v": 1, "target": "x.py"}))
            backend = install_sim(FakeBackend(), plan_path=str(plan_path), env={"KEEL_QUIET": "1"})
            outcome = backend.execute({"target": "t"}, lambda a: {"status": "ok", "payload": "live"})
            self.assertEqual(outcome, {"status": "ok", "payload": "live"})

    def test_unreadable_plan_exits_loudly(self) -> None:
        with self.assertRaises(SystemExit):
            install_sim(FakeBackend(), plan_path="/does/not/exist.json", env={"KEEL_QUIET": "1"})


class SimCrashExitCodeTest(unittest.TestCase):
    def test_is_128_plus_sigkill(self) -> None:
        self.assertEqual(SIM_CRASH_EXIT_CODE, 137)


if __name__ == "__main__":
    unittest.main()
