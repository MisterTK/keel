"""Tier 2 flow designation and the durable-flow run path (dx-spec §1 Level 2).

Three layers:
  * pure parsing/matching (`extract_flow_entrypoints`, `match_flow`),
  * the `run_as_flow` orchestration + time/random virtualization against a fake
    backend (no native module needed — CI's no-wheel path),
  * the native binding replay round-trip (skips without the built `keel_core`).
"""

from __future__ import annotations

import sys
import tempfile
import textwrap
import unittest
from pathlib import Path

from keel import _flow
from keel._policy import FlowEntrypoint, extract_flow_entrypoints

try:
    import keel_core  # noqa: F401

    _NATIVE = True
except ImportError:
    _NATIVE = False


class ExtractFlowEntrypointsTest(unittest.TestCase):
    def test_parses_py_module_function(self) -> None:
        entries = extract_flow_entrypoints({"flows": {"entrypoints": ["py:pipeline:main"]}})
        self.assertEqual(entries, [FlowEntrypoint("py:pipeline:main", "pipeline", "main")])

    def test_skips_malformed_and_non_py(self) -> None:
        got = extract_flow_entrypoints(
            {"flows": {"entrypoints": ["py:nofunc", "js:x:y", "py:m:f", 7]}}
        )
        self.assertEqual([e.raw for e in got], ["py:m:f"])

    def test_absent_flows_is_empty(self) -> None:
        self.assertEqual(extract_flow_entrypoints({}), [])
        self.assertEqual(extract_flow_entrypoints({"flows": {}}), [])

    def test_glob_module_with_concrete_function(self) -> None:
        entries = extract_flow_entrypoints({"flows": {"entrypoints": ["py:jobs.*:run"]}})
        self.assertEqual(entries, [FlowEntrypoint("py:jobs.*:run", "jobs.*", "run")])

    def test_glob_shorthand_defaults_function_to_main(self) -> None:
        entries = extract_flow_entrypoints({"flows": {"entrypoints": ["py:pipeline.*"]}})
        self.assertEqual(entries, [FlowEntrypoint("py:pipeline.*", "pipeline.*", "main")])

    def test_glob_in_function_position_is_skipped(self) -> None:
        # The flow body must always be a concrete, named function.
        got = extract_flow_entrypoints({"flows": {"entrypoints": ["py:jobs.ingest:*"]}})
        self.assertEqual(got, [])

    def test_concrete_module_without_function_is_still_skipped(self) -> None:
        # Unchanged v0.1 rule: a colon-less, non-glob entry has no function to
        # call and is not guessed.
        self.assertEqual(extract_flow_entrypoints({"flows": {"entrypoints": ["py:pipeline"]}}), [])


class MatchFlowTest(unittest.TestCase):
    def test_matches_by_file_stem(self) -> None:
        entries = [FlowEntrypoint("py:pipeline:main", "pipeline", "main")]
        self.assertEqual(_flow.match_flow("/tmp/pipeline.py", entries), entries[0])
        self.assertIsNone(_flow.match_flow("/tmp/other.py", entries))

    def test_no_entries_no_match(self) -> None:
        self.assertIsNone(_flow.match_flow("/tmp/pipeline.py", []))

    def test_dotted_module_matches_only_its_path_not_a_bare_stem(self) -> None:
        # A dotted-module entrypoint must NOT be entered by an unrelated script
        # that merely shares the last name component (would resume a foreign flow).
        entries = [FlowEntrypoint("py:jobs.pipeline:main", "jobs.pipeline", "main")]
        self.assertEqual(_flow.match_flow("/app/jobs/pipeline.py", entries), entries[0])
        self.assertIsNone(_flow.match_flow("/scratch/pipeline.py", entries))
        self.assertIsNone(_flow.match_flow("/app/other/pipeline.py", entries))

    def test_glob_entry_resolves_to_concrete_matched_module(self) -> None:
        entries = [FlowEntrypoint("py:pipeline.*:main", "pipeline.*", "main")]
        got = _flow.match_flow("/app/pipeline/ingest.py", entries)
        self.assertIsNotNone(got)
        self.assertEqual(got.module, "pipeline.ingest")
        self.assertEqual(got.raw, "py:pipeline.ingest:main")
        self.assertEqual(got.function, "main")
        self.assertEqual(got.via, "py:pipeline.*:main")

    def test_two_scripts_under_one_glob_get_independent_identities(self) -> None:
        entries = [FlowEntrypoint("py:jobs.*:run", "jobs.*", "run")]
        a = _flow.match_flow("/app/jobs/ingest.py", entries)
        b = _flow.match_flow("/app/jobs/export.py", entries)
        self.assertNotEqual(a.raw, b.raw)
        self.assertEqual(a.raw, "py:jobs.ingest:run")
        self.assertEqual(b.raw, "py:jobs.export:run")

    def test_shortest_glob_candidate_wins(self) -> None:
        # For .../jobs/nightly.py, candidates are "nightly" then "jobs.nightly"
        # (shortest first) — a glob matching the bare stem wins.
        entries = [FlowEntrypoint("py:*:main", "*", "main")]
        got = _flow.match_flow("/app/jobs/nightly.py", entries)
        self.assertEqual(got.module, "nightly")

    def test_concrete_entry_wins_over_a_matching_glob(self) -> None:
        entries = [
            FlowEntrypoint("py:pipeline.*:main", "pipeline.*", "main"),
            FlowEntrypoint("py:pipeline.ingest:special", "pipeline.ingest", "special"),
        ]
        got = _flow.match_flow("/app/pipeline/ingest.py", entries)
        self.assertEqual(got, entries[1])
        self.assertIsNone(got.via)

    def test_glob_stops_at_the_first_non_identifier_path_component(self) -> None:
        # "my-jobs" is not a valid Python identifier, so no dotted reading can
        # extend past it outward — a glob that would need a component beyond it
        # can never be reached, even though a shallower one still matches.
        shallow = [FlowEntrypoint("py:*.ingest:main", "*.ingest", "main")]
        self.assertEqual(
            _flow.match_flow("/app/my-jobs/sub/ingest.py", shallow).module, "sub.ingest"
        )
        deep = [FlowEntrypoint("py:*.sub.ingest:main", "*.sub.ingest", "main")]
        self.assertIsNone(_flow.match_flow("/app/my-jobs/sub/ingest.py", deep))

    def test_glob_does_not_match_a_non_identifier_stem(self) -> None:
        entries = [FlowEntrypoint("py:*:main", "*", "main")]
        self.assertIsNone(_flow.match_flow("/app/123abc.py", entries))

    def test_glob_ignored_for_non_python_target(self) -> None:
        entries = [FlowEntrypoint("py:*:main", "*", "main")]
        self.assertIsNone(_flow.match_flow("/app/script.txt", entries))


class _FakeFlowBackend:
    """A native-shaped double: records enter/exit and routes execute + value
    steps, so `run_as_flow` is testable without the compiled core."""

    def __init__(self, replay: bool = False, persistent: bool = True) -> None:
        self.entered: list[tuple] = []
        self.exited: list[str] = []
        self.executed = 0
        self.times: list[int] = []
        self._replay = replay
        # Models a native core with a journal attached (Tier 2 available). Set
        # False to model a native core with no journal (the fix-#2 gate).
        self.persistent = persistent

    def enter_flow(self, entrypoint, args_hash, code_hash=None, explicit_key=None, lease_ms=None):
        self.entered.append((entrypoint, args_hash, code_hash, lease_ms))
        status = "completed" if self._replay else "running"
        return {"flow_id": "fid-1", "status": status, "replay": self._replay}

    def exit_flow(self, status: str) -> None:
        self.exited.append(status)

    def execute(self, request, effect):
        self.executed += 1
        result = effect(0)
        return {"result": result.get("status", "ok"), "payload": result.get("payload")}

    def journal_time(self, key: str, now_ms: int) -> int:
        self.times.append(now_ms)
        return 424242 if self._replay else now_ms

    def journal_random(self, key: str, data: bytes) -> bytes:
        return data


def _write_module(dir_: Path, name: str, body: str) -> FlowEntrypoint:
    (dir_ / f"{name}.py").write_text(textwrap.dedent(body))
    return FlowEntrypoint(f"py:{name}:main", name, "main")


class RunAsFlowTest(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self._tmp.name)
        sys.path.insert(0, str(self.dir))
        self._added_modules: list[str] = []

    def tearDown(self) -> None:
        try:
            sys.path.remove(str(self.dir))
        except ValueError:
            pass
        for m in self._added_modules:
            sys.modules.pop(m, None)
        self._tmp.cleanup()

    def _module(self, name: str, body: str) -> FlowEntrypoint:
        self._added_modules.append(name)
        return _write_module(self.dir, name, body)

    def test_enters_runs_and_completes(self) -> None:
        entry = self._module(
            "flowmod_ok",
            """
            import time
            from keel import _runtime

            def main():
                backend = _runtime.get_backend()
                for i in range(3):
                    backend.execute({"v": 1, "target": "t", "op": "t"}, lambda a: {"status": "ok"})
                # a virtualized read inside the flow
                globals()["SEEN_TIME"] = time.time()
            """,
        )
        backend = _FakeFlowBackend()
        from keel import _runtime

        _runtime.set_runtime(backend, None)
        try:
            _flow.run_as_flow(
                str(self.dir / "flowmod_ok.py"), entry, backend, [], env={"KEEL_QUIET": "1"}
            )
        finally:
            _runtime.clear_runtime()
        self.assertEqual(len(backend.entered), 1)
        self.assertEqual(backend.executed, 3)
        self.assertEqual(backend.exited, ["completed"])
        self.assertEqual(len(backend.times), 1, "time.time was virtualized in-flow")

    def test_failure_marks_flow_failed_and_reraises(self) -> None:
        entry = self._module(
            "flowmod_boom",
            """
            def main():
                raise ValueError("boom")
            """,
        )
        backend = _FakeFlowBackend()
        with self.assertRaises(ValueError):
            _flow.run_as_flow(
                str(self.dir / "flowmod_boom.py"), entry, backend, [], env={"KEEL_QUIET": "1"}
            )
        self.assertEqual(backend.exited, ["failed"])

    def test_clean_sys_exit_completes_not_fails(self) -> None:
        # sys.exit(0) is the ordinary success exit — it must complete the flow,
        # not mark it 'failed' (which would march a working script to 'dead').
        entry = self._module(
            "flowmod_exit0",
            """
            import sys
            def main():
                sys.exit(0)
            """,
        )
        backend = _FakeFlowBackend()
        with self.assertRaises(SystemExit):
            _flow.run_as_flow(
                str(self.dir / "flowmod_exit0.py"), entry, backend, [], env={"KEEL_QUIET": "1"}
            )
        self.assertEqual(backend.exited, ["completed"])

    def test_replayed_flow_is_not_demoted_on_error(self) -> None:
        # A rerun of an already-COMPLETED flow that raises (e.g. a replay-miss
        # after a code change) must NOT be stamped 'failed' — that would re-open a
        # finished flow for live re-execution.
        entry = self._module(
            "flowmod_replay_err",
            """
            def main():
                raise RuntimeError("changed code / replay miss")
            """,
        )
        backend = _FakeFlowBackend(replay=True)  # already completed → replay path
        with self.assertRaises(RuntimeError):
            _flow.run_as_flow(
                str(self.dir / "flowmod_replay_err.py"), entry, backend, [], env={"KEEL_QUIET": "1"}
            )
        self.assertEqual(backend.exited, [], "completed flow must not be demoted to failed")

    def test_time_random_restored_after_flow(self) -> None:
        import random
        import time

        orig_time, orig_random = time.time, random.random
        entry = self._module(
            "flowmod_restore",
            """
            def main():
                pass
            """,
        )
        backend = _FakeFlowBackend()
        _flow.run_as_flow(
            str(self.dir / "flowmod_restore.py"), entry, backend, [], env={"KEEL_QUIET": "1"}
        )
        self.assertIs(time.time, orig_time, "time.time restored on flow exit")
        self.assertIs(random.random, orig_random, "random.random restored on flow exit")

    def test_stub_backend_is_precise_unsupported_error(self) -> None:
        entry = FlowEntrypoint("py:pipeline:main", "pipeline", "main")

        class _StubLike:  # no enter_flow/exit_flow
            def execute(self, request, effect):  # pragma: no cover - not reached
                return {}

        with self.assertRaises(SystemExit) as ctx:
            _flow.run_as_flow("/tmp/pipeline.py", entry, _StubLike(), [], env={"KEEL_QUIET": "1"})
        self.assertEqual(ctx.exception.code, 1)

    def test_native_backend_without_journal_is_precise_error_before_enter(self) -> None:
        """Carried fix #2: a native-shaped backend with no journal must be
        refused by the FRONT END (config-level KEEL-E005), before `enter_flow`
        is ever called — so the backend's last-resort KEEL-E040 is unreachable
        from `keel run`."""
        entry = FlowEntrypoint("py:pipeline:main", "pipeline", "main")
        backend = _FakeFlowBackend(persistent=False)
        with self.assertRaises(SystemExit) as ctx:
            _flow.run_as_flow("/tmp/pipeline.py", entry, backend, [], env={"KEEL_QUIET": "1"})
        self.assertEqual(ctx.exception.code, 1)
        self.assertEqual(backend.entered, [], "enter_flow must NOT be reached without a journal")


@unittest.skipUnless(_NATIVE, "keel_core native module not built (maturin develop in crates/keel-py)")
class NativeFlowReplayTest(unittest.TestCase):
    def _core(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        core = keel_core.KeelCore(journal_path=str(Path(self._tmp.name) / "journal.db"))
        core.configure({})
        return core

    def test_completed_flow_replays_without_refiring_effects(self) -> None:
        core = self._core()
        fires = {"n": 0}

        def eff(_attempt):
            fires["n"] += 1
            return {"status": "ok", "payload": {"i": fires["n"]}}

        def run_once():
            core.enter_flow("py:pipeline:main", "ah-1", code_hash="ch-1")
            for i in range(3):
                out = core.execute(
                    {"v": 1, "target": "api.x", "op": "api.x", "args_hash": f"h{i}", "idempotent": True},
                    eff,
                )
                self.assertEqual(out["result"], "ok")
            t = core.journal_time("py:time.time#-", 1783728000)
            core.exit_flow("completed")
            return t

        # Run 1: 3 live effects + a recorded time.
        first = run_once()
        self.assertEqual(fires["n"], 3)
        self.assertEqual(first, 1783728000)

        # Run 2: completed → pure replay. No effect re-fires; recorded values
        # (payloads, time) are substituted.
        info = core.enter_flow("py:pipeline:main", "ah-1", code_hash="ch-1")
        self.assertEqual(info["status"], "completed")
        self.assertTrue(info["replay"])
        for i in range(3):
            out = core.execute(
                {"v": 1, "target": "api.x", "op": "api.x", "args_hash": f"h{i}", "idempotent": True},
                eff,
            )
            self.assertEqual(out["payload"], {"i": i + 1})
        replayed_time = core.journal_time("py:time.time#-", 9999)
        core.exit_flow("completed")
        self.assertEqual(fires["n"], 3, "replay fired no effects")
        self.assertEqual(replayed_time, 1783728000, "time replayed")

    def test_flow_requires_a_journal(self) -> None:
        core = keel_core.KeelCore()  # in-memory, no journal
        core.configure({})
        with self.assertRaises(keel_core.KeelCoreError) as ctx:
            core.enter_flow("py:pipeline:main", "ah-1")
        self.assertEqual(ctx.exception.code, "KEEL-E040")

    def test_async_effect_in_flow_is_refused(self) -> None:
        """Carried fix #1: an async intercepted call while a flow is open must be
        refused (KEEL-E005), synchronously, rather than silently downgraded to
        Tier 1 by running on the bare engine outside the FlowHandle."""
        core = self._core()

        async def eff(_attempt):  # pragma: no cover - never awaited (guard fires first)
            return {"status": "ok"}

        core.enter_flow("py:pipeline:main", "ah-async", code_hash="ch-1")
        try:
            with self.assertRaises(keel_core.KeelCoreError) as ctx:
                core.execute_async(
                    {"v": 1, "target": "api.x", "op": "api.x", "idempotent": True}, eff
                )
            self.assertEqual(ctx.exception.code, "KEEL-E005")
            self.assertIn("durable flow", str(ctx.exception).lower())
        finally:
            core.exit_flow("completed")


if __name__ == "__main__":
    unittest.main()
