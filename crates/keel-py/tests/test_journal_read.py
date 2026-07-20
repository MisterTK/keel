#!/usr/bin/env python3
"""End-to-end test for the `keel_core` PyO3 module's cross-flow journal read
surface: `KeelCore.flows_by_entrypoint`/`KeelCore.steps_for_flow`.

These are the read methods a long-lived, out-of-process caller (e.g. a
session-service style reader — issue #15's design doc §3.2) uses to
reconstruct history spanning many past, already-closed flows — never the
caller's own live handle. Exercises them end to end on a real temp-file
journal: two flows entered/exited under the same entrypoint (one with a
single step, one with two), plus a third flow under a DIFFERENT entrypoint
that must never leak into the first entrypoint's results.

Requires the module to be built and importable (`maturin develop` in
crates/keel-py). Run with the venv's Python:
    python crates/keel-py/tests/test_journal_read.py
Exit code 0 on success.
"""

from __future__ import annotations

import sys
import tempfile
from pathlib import Path

import keel_core

ENTRYPOINT = "py:pipeline.ingest:main"
OTHER_ENTRYPOINT = "py:other.module:main"


def _request(args_hash: str) -> dict:
    return {
        "v": 1,
        "target": "api.example.internal",
        "op": "GET api.example.internal/item",
        "idempotent": True,
        "args_hash": args_hash,
    }


def test_flows_by_entrypoint_and_steps_for_flow_round_trip() -> None:
    with tempfile.TemporaryDirectory() as tmp:
        journal_path = str(Path(tmp) / "journal.db")
        core = keel_core.KeelCore(journal_path=journal_path)
        core.configure({})

        # Flow 1 (identity: ENTRYPOINT + "args-1"): a single journaled step.
        info1 = core.enter_flow(ENTRYPOINT, "args-1")
        flow_id_1 = info1["flow_id"]
        assert info1["status"] == "running", info1
        assert info1["replay"] is False, info1

        out1 = core.execute(
            _request("q1"), lambda attempt: {"status": "ok", "payload": {"n": 1}}
        )
        assert out1["result"] == "ok", out1
        core.exit_flow("completed")

        # Flow 2 (identity: ENTRYPOINT + "args-2", a DIFFERENT flow_id): two
        # journaled steps, intervening between flow 1 and flow 3 below.
        info2 = core.enter_flow(ENTRYPOINT, "args-2")
        flow_id_2 = info2["flow_id"]
        assert flow_id_2 != flow_id_1, "different args_hash must mint a different flow"

        out2a = core.execute(
            _request("q2a"),
            lambda attempt: {"status": "ok", "payload": {"n": 2, "who": "a"}},
        )
        assert out2a["result"] == "ok", out2a
        out2b = core.execute(
            _request("q2b"),
            lambda attempt: {"status": "ok", "payload": {"n": 3, "who": "b"}},
        )
        assert out2b["result"] == "ok", out2b
        core.exit_flow("completed")

        # Flow 3: a DIFFERENT entrypoint entirely. Must not appear in
        # flows_by_entrypoint(ENTRYPOINT) below.
        other_info = core.enter_flow(OTHER_ENTRYPOINT, "args-x")
        core.exit_flow("completed")

        # --- flows_by_entrypoint: every status, ordered by created_at ---
        flows = core.flows_by_entrypoint(ENTRYPOINT)
        ids = [f["flow_id"] for f in flows]
        assert ids == [flow_id_1, flow_id_2], ids
        for f in flows:
            assert f["entrypoint"] == ENTRYPOINT, f
            assert f["status"] == "completed", f
            assert isinstance(f["created_at"], int), f
            assert isinstance(f["updated_at"], int), f
        assert flows[0]["args_hash"] == "args-1", flows[0]
        assert flows[1]["args_hash"] == "args-2", flows[1]

        other_ids = [f["flow_id"] for f in core.flows_by_entrypoint(OTHER_ENTRYPOINT)]
        assert other_info["flow_id"] in other_ids
        assert flow_id_1 not in other_ids and flow_id_2 not in other_ids

        assert core.flows_by_entrypoint("py:nonexistent:main") == []

        # --- steps_for_flow: every step in seq order, payload decoded ---
        # `enter_flow` itself journals a leading `kind: "marker"` attempt-
        # counter step (issue #14) before any `execute()` step — steps_for_flow
        # returns EVERY step, so that marker is seq 0 here, ahead of the
        # effect step(s) this test drove.
        steps1 = core.steps_for_flow(flow_id_1)
        assert len(steps1) == 2, steps1
        assert steps1[0]["kind"] == "marker", steps1[0]
        effects1 = [s for s in steps1 if s["kind"] == "effect"]
        assert len(effects1) == 1, steps1
        assert effects1[0]["status"] == "ok", effects1[0]
        assert effects1[0]["payload"] == {"n": 1}, effects1[0]

        steps2 = core.steps_for_flow(flow_id_2)
        assert len(steps2) == 3, steps2
        assert steps2[0]["kind"] == "marker", steps2[0]
        effects2 = [s for s in steps2 if s["kind"] == "effect"]
        assert len(effects2) == 2, steps2
        assert effects2[0]["payload"] == {"n": 2, "who": "a"}, effects2[0]
        assert effects2[1]["payload"] == {"n": 3, "who": "b"}, effects2[1]
        for s in effects2:
            assert s["status"] == "ok", s
            assert isinstance(s["started_at"], int), s
            assert isinstance(s["ended_at"], int), s

        assert core.steps_for_flow("01NONEXISTENTFLOW00000000") == []

        print("journal read: flows_by_entrypoint/steps_for_flow round-trip OK")


def test_no_journal_raises_e040() -> None:
    """Both methods refuse loudly (KEEL-E040), never silently, on an
    in-memory core with no journal attached — same taxonomy slot/message
    style as `enter_flow`'s own no-journal refusal."""
    core = keel_core.KeelCore()  # no journal_path -> in-memory, no journal

    try:
        core.flows_by_entrypoint(ENTRYPOINT)
    except keel_core.KeelCoreError as e:
        assert e.code == "KEEL-E040", e.code
    else:
        raise AssertionError("expected KEEL-E040 without a journal")

    try:
        core.steps_for_flow("01SOMEFLOW0000000000000001")
    except keel_core.KeelCoreError as e:
        assert e.code == "KEEL-E040", e.code
    else:
        raise AssertionError("expected KEEL-E040 without a journal")

    print("journal read: no-journal E040 OK")


def main() -> int:
    test_flows_by_entrypoint_and_steps_for_flow_round_trip()
    test_no_journal_raises_e040()
    print("journal read tests: 2/2 passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
