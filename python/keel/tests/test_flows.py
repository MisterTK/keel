"""Tier 2 flow designation and the durable-flow run path (dx-spec §1 Level 2).

Four layers:
  * pure parsing/matching (`extract_flow_entrypoints`, `match_flow`),
  * the `run_as_flow` orchestration + time/random virtualization against a fake
    backend (no native module needed — CI's no-wheel path), including an
    `async def` flow body run via `asyncio.run`,
  * the native binding replay round-trip (skips without the built `keel_core`),
  * the async `execute_step` bridge: concurrent `asyncio.gather`ed effects
    inside one flow serialize in admission order and a crash (dropped handle,
    no `exit_flow`) resumes correctly on a fresh `KeelCore` over the same
    journal — the kill-9 shape, mirroring
    `crash_after_step_three_resumes_substituting_completed_steps` in
    `crates/keel-core/tests/flows.rs`.
"""

from __future__ import annotations

import asyncio
import gc
import sqlite3
import sys
import tempfile
import textwrap
import time
import unittest
from pathlib import Path
from typing import Any

from keel import _flow
from keel._policy import FlowEntrypoint, extract_flow_entrypoints

try:
    import keel_core  # noqa: F401

    _NATIVE = True
except ImportError:
    _NATIVE = False


def _pack_bare_str_map(pairs: list[tuple[str, str]]) -> bytes:
    """Hand-rolled bare MessagePack for a small string->string map — the exact
    bytes `crates/keel-core/src/flow.rs`'s `decode_payload` falls back to
    decoding when a payload carries no `keel.step/v1` schema tag (the "legacy
    bare messagepack still decodes" path its own unit test pins). Fixstr-only
    (every key/value here is well under 32 bytes), so a single-byte header per
    string suffices."""

    def fixstr(s: str) -> bytes:
        raw = s.encode("utf-8")
        assert len(raw) < 32, "fixstr only; not needed for test-sized values"
        return bytes([0xA0 | len(raw)]) + raw

    out = bytes([0x80 | len(pairs)])  # fixmap header
    for k, v in pairs:
        out += fixstr(k) + fixstr(v)
    return out


def _inject_running_step(
    journal_path: str, flow_id: str, *, seq: int, step_key: str, idempotency_key: str
) -> None:
    """Directly journal a `running` (unterminated) step record carrying an
    adapter-injected idempotency key — models a crash mid-effect: the exact
    shape `FlowHandle::run_live` writes BEFORE firing the effect (a real crash
    between that write and the terminal record leaves precisely this row).
    Same technique `conformance/scenarios`' JSON-driven interpreter uses via
    its `inject_running` field (`crates/keel-core/tests/flows_conformance.rs`),
    applied directly against the on-disk journal.db so this test proves the
    REAL PyO3 binding surface (`recorded_idempotency_key`,
    `execute`/`execute_async`'s `idempotency_key` parameter) rather than
    re-deriving the core-level proof `crates/keel-core/tests/idempotency.rs`
    already carries."""
    payload = _pack_bare_str_map([("idempotency_key", idempotency_key)])
    conn = sqlite3.connect(journal_path)
    try:
        conn.execute(
            "INSERT INTO steps (flow_id, seq, step_key, kind, attempt, outcome, payload, "
            "error_class, started_at, ended_at) VALUES (?,?,?,?,?,?,?,?,?,?)",
            (flow_id, seq, step_key, "effect", 0, "running", payload, None, int(time.time() * 1000), None),
        )
        conn.commit()
    finally:
        conn.close()


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

    def test_dotted_qualname_function_slot_parses(self) -> None:
        # The ADK Runner-flow designation (keel.packs.adk_pack.RUNNER_FLOW_ENTRYPOINT)
        # depends on rsplit-based parsing admitting a dotted qualname in the
        # function slot — fence it against future grammar/parser changes.
        out = extract_flow_entrypoints(
            {"flows": {"entrypoints": ["py:google.adk.runners:Runner.run_async"]}}
        )
        self.assertEqual(len(out), 1)
        self.assertEqual(out[0].module, "google.adk.runners")
        self.assertEqual(out[0].function, "Runner.run_async")
        self.assertEqual(out[0].raw, "py:google.adk.runners:Runner.run_async")


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

    def test_async_flow_body_runs_via_asyncio_run(self) -> None:
        # An `async def` entrypoint is driven by `asyncio.run`, not called
        # bare (which would just build a coroutine object and never run it).
        entry = self._module(
            "flowmod_async_ok",
            """
            import asyncio

            async def main():
                await asyncio.sleep(0)
                globals()["RAN"] = True
            """,
        )
        backend = _FakeFlowBackend()
        _flow.run_as_flow(
            str(self.dir / "flowmod_async_ok.py"), entry, backend, [], env={"KEEL_QUIET": "1"}
        )
        self.assertEqual(backend.exited, ["completed"])
        self.assertTrue(sys.modules["flowmod_async_ok"].RAN, "the coroutine actually ran")

    def test_async_flow_body_failure_marks_flow_failed_and_reraises(self) -> None:
        entry = self._module(
            "flowmod_async_boom",
            """
            async def main():
                raise ValueError("async boom")
            """,
        )
        backend = _FakeFlowBackend()
        with self.assertRaises(ValueError):
            _flow.run_as_flow(
                str(self.dir / "flowmod_async_boom.py"), entry, backend, [], env={"KEEL_QUIET": "1"}
            )
        self.assertEqual(backend.exited, ["failed"])

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

    def test_idempotency_key_recorded_on_crash_survives_resume(self) -> None:
        """contracts/adapter-pack.md "Idempotency-key injection" rule 3,
        through the REAL PyO3 binding (the gap this test closes: `execute`/
        `execute_async` did not expose `idempotency_key`, and there was no
        binding-level `recorded_idempotency_key` peek at all). Step 1 completes
        normally with key ``ik-1``; step 2's crash is modeled by directly
        journaling its `running` record (``_inject_running_step`` — see its
        docstring) carrying key ``ik-2``. On resume: step 1's peek misses (it is
        terminal) and its effect never re-fires; step 2's peek must resurface
        ``ik-2`` — and the re-executed live effect, injected with THAT peeked
        key, must actually run with the SAME key as the crashed attempt, not
        merely with *a* key."""
        core1 = self._core()
        journal_path = str(Path(self._tmp.name) / "journal.db")
        entrypoint = "py:billing.charge:main"
        args_hash = "ah-idem-native"
        flow_id = f"{entrypoint}#{args_hash}#"

        core1.enter_flow(entrypoint, args_hash, code_hash="ch-1")
        step1_key = "api.pay.example#c1"
        step2_key = "api.pay.example#c2"
        self.assertIsNone(core1.recorded_idempotency_key(step1_key))
        out1 = core1.execute(
            {"v": 1, "target": "api.pay.example", "op": "POST x", "args_hash": "c1", "idempotent": True},
            lambda _a: {"status": "ok", "payload": {"charge": "ch_1"}},
            idempotency_key="ik-1",
        )
        self.assertEqual(out1["result"], "ok")
        self.assertIsNone(core1.recorded_idempotency_key(step2_key))

        # Crash: step 2's `running` record (carrying ik-2) is journaled
        # directly, never through a live `execute` call — modeling the process
        # dying between the running-write and the terminal outcome.
        _inject_running_step(
            journal_path, flow_id, seq=2, step_key=step2_key, idempotency_key="ik-2"
        )
        del core1
        gc.collect()

        core2 = keel_core.KeelCore(journal_path=journal_path)
        core2.configure({})
        info = core2.enter_flow(entrypoint, args_hash, code_hash="ch-1")
        self.assertFalse(info["replay"], "an uncompleted flow resumes live, not a pure replay")

        # Step 1 is terminal: the peek misses, and a resumed re-execution
        # (with a DIFFERENT, would-be-wrong key) is substituted, not re-fired.
        self.assertIsNone(core2.recorded_idempotency_key(step1_key))
        fires = {"n": 0}

        def eff1(_a: int) -> dict[str, Any]:
            fires["n"] += 1
            return {"status": "ok", "payload": {"charge": "never"}}

        out1b = core2.execute(
            {"v": 1, "target": "api.pay.example", "op": "POST x", "args_hash": "c1", "idempotent": True},
            eff1,
            idempotency_key="ik-should-be-ignored",
        )
        self.assertEqual(out1b["payload"], {"charge": "ch_1"}, "a terminal step is substituted")
        self.assertEqual(fires["n"], 0, "a substituted step must not fire its effect")

        # Step 2: the crashed `running` record resurfaces its key — the
        # load-bearing assertion is exact equality with the crashed attempt's
        # key, not merely that SOME key came back.
        peeked = core2.recorded_idempotency_key(step2_key)
        self.assertEqual(
            peeked, "ik-2", "the peek must resurface the SAME key the crashed attempt recorded"
        )

        out2 = core2.execute(
            {"v": 1, "target": "api.pay.example", "op": "POST x", "args_hash": "c2", "idempotent": True},
            lambda _a: {"status": "ok", "payload": {"charge": "ch_2"}},
            idempotency_key=peeked,
        )
        self.assertEqual(out2["result"], "ok")
        self.assertEqual(out2["attempts"], 1)
        core2.exit_flow("completed")

    def test_flow_requires_a_journal(self) -> None:
        core = keel_core.KeelCore()  # in-memory, no journal
        core.configure({})
        with self.assertRaises(keel_core.KeelCoreError) as ctx:
            core.enter_flow("py:pipeline:main", "ah-1")
        self.assertEqual(ctx.exception.code, "KEEL-E040")

    def test_async_effect_in_flow_is_journaled_and_replays(self) -> None:
        """The async execute_step bridge: an awaited intercepted call while a
        flow is open routes through the SAME FlowHandle a synchronous call
        would, so it is journaled and — on a rerun of a completed flow —
        replayed without re-firing (mirrors
        test_completed_flow_replays_without_refiring_effects, async leg)."""
        core = self._core()
        fires = {"n": 0}

        async def eff(_attempt):
            fires["n"] += 1
            return {"status": "ok", "payload": {"i": fires["n"]}}

        async def run_once() -> None:
            core.enter_flow("py:pipeline:main", "ah-async-ok", code_hash="ch-1")
            for i in range(3):
                out = await core.execute_async(
                    {
                        "v": 1,
                        "target": "api.x",
                        "op": "api.x",
                        "args_hash": f"h{i}",
                        "idempotent": True,
                    },
                    eff,
                )
                self.assertEqual(out["result"], "ok")
            core.exit_flow("completed")

        asyncio.run(run_once())
        self.assertEqual(fires["n"], 3)

        async def replay_once() -> list[object]:
            info = core.enter_flow("py:pipeline:main", "ah-async-ok", code_hash="ch-1")
            self.assertTrue(info["replay"])
            payloads = []
            for i in range(3):
                out = await core.execute_async(
                    {
                        "v": 1,
                        "target": "api.x",
                        "op": "api.x",
                        "args_hash": f"h{i}",
                        "idempotent": True,
                    },
                    eff,
                )
                payloads.append(out["payload"])
            core.exit_flow("completed")
            return payloads

        payloads = asyncio.run(replay_once())
        self.assertEqual(payloads, [{"i": 1}, {"i": 2}, {"i": 3}])
        self.assertEqual(fires["n"], 3, "replay fired no async effects")

    def test_concurrent_async_effects_serialize_in_admission_order(self) -> None:
        """Normative (conformance/README.md "Async steps inside a flow"): the
        open flow handle admits one step at a time, so `asyncio.gather`ed
        effects never run concurrently and are journaled in the order their
        calls reach the handle. Creation is staggered with small real sleeps
        so each call is already admitted (or queued behind an admitted call)
        before the next is even created — this is a genuinely real-time async
        bridge (no virtual clock on this path), so a real sleep is required to
        pin admission order deterministically rather than racing three tasks
        onto a multi-threaded tokio runtime."""
        core = self._core()
        active = {"n": 0}
        max_active = {"n": 0}
        finished: list[int] = []

        def make_eff(i: int, sleep_s: float):
            async def eff(_attempt):
                active["n"] += 1
                max_active["n"] = max(max_active["n"], active["n"])
                await asyncio.sleep(sleep_s)
                active["n"] -= 1
                finished.append(i)
                return {"status": "ok", "payload": {"i": i}}

            return eff

        async def call(i: int, sleep_s: float):
            return await core.execute_async(
                {
                    "v": 1,
                    "target": "api.x",
                    "op": "api.x",
                    "args_hash": f"h{i}",
                    "idempotent": True,
                },
                make_eff(i, sleep_s),
            )

        async def run_once():
            core.enter_flow("py:pipeline:main", "ah-async-concurrent", code_hash="ch-1")
            # Sleep durations DECREASE with index: if effects ran unserialized
            # they would FINISH out of admission order (2, 1, 0); serialization
            # instead forces each to finish before the next is even admitted.
            t0 = asyncio.ensure_future(call(0, 0.03))
            await asyncio.sleep(0.01)
            t1 = asyncio.ensure_future(call(1, 0.02))
            await asyncio.sleep(0.01)
            t2 = asyncio.ensure_future(call(2, 0.01))
            results = await asyncio.gather(t0, t1, t2)
            core.exit_flow("completed")
            return results

        results = asyncio.run(run_once())
        self.assertEqual(max_active["n"], 1, "no two admitted effects ever run concurrently")
        self.assertEqual(finished, [0, 1, 2], "each effect finishes before the next is admitted")
        self.assertEqual([r["payload"] for r in results], [{"i": 0}, {"i": 1}, {"i": 2}])

    def test_async_flow_crash_and_resume_substitutes_completed_steps(self) -> None:
        """The kill-9 shape for the async bridge: two concurrent async steps
        complete and are journaled, the handle is dropped WITHOUT `exit_flow`
        (the crash `FlowHandle::drop` documents — left `running` with its
        lease), and a FRESH `KeelCore` opened over the same on-disk journal
        resumes the flow: the two completed steps substitute (their effects
        never re-fire) and a third, new step runs live. Mirrors
        `crash_after_step_three_resumes_substituting_completed_steps` in
        crates/keel-core/tests/flows.rs, adapted to the async bridge."""
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        journal_path = str(Path(tmp.name) / "journal.db")
        fires = {"n": 0}

        def make_eff(payload_i: int):
            async def eff(_attempt):
                fires["n"] += 1
                return {"status": "ok", "payload": {"i": payload_i}}

            return eff

        async def call(core: Any, i: int):
            return await core.execute_async(
                {
                    "v": 1,
                    "target": "api.x",
                    "op": "api.x",
                    "args_hash": f"h{i}",
                    "idempotent": True,
                },
                make_eff(i),
            )

        async def run1() -> list[object]:
            core1 = keel_core.KeelCore(journal_path=journal_path)
            core1.configure({})
            core1.enter_flow("py:pipeline:main", "ah-async-crash", code_hash="ch-1")
            t0 = asyncio.ensure_future(call(core1, 0))
            await asyncio.sleep(0.01)
            t1 = asyncio.ensure_future(call(core1, 1))
            results = await asyncio.gather(t0, t1)
            return results
            # core1 (and its FlowHandle) fall out of scope here uncompleted —
            # no exit_flow — modeling a `kill -9`: the flow stays `running`.

        first = asyncio.run(run1())
        gc.collect()  # deterministically drop core1's FlowHandle/journal now
        self.assertEqual(fires["n"], 2, "two live steps before the crash")
        self.assertEqual([r["payload"] for r in first], [{"i": 0}, {"i": 1}])

        async def run2() -> list[object]:
            core2 = keel_core.KeelCore(journal_path=journal_path)
            core2.configure({})
            info = core2.enter_flow("py:pipeline:main", "ah-async-crash", code_hash="ch-1")
            self.assertFalse(info["replay"], "an uncompleted flow resumes live, not a pure replay")
            t0 = asyncio.ensure_future(call(core2, 0))
            await asyncio.sleep(0.01)
            t1 = asyncio.ensure_future(call(core2, 1))
            await asyncio.sleep(0.01)
            t2 = asyncio.ensure_future(call(core2, 2))
            results = await asyncio.gather(t0, t1, t2)
            core2.exit_flow("completed")
            return results

        second = asyncio.run(run2())
        self.assertEqual(fires["n"], 3, "steps 1-2 substituted from the journal; only step 3 ran live")
        self.assertEqual([r["payload"] for r in second], [{"i": 0}, {"i": 1}, {"i": 2}])


if __name__ == "__main__":
    unittest.main()
