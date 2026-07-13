"""The `langgraph_pack` module: node-execution wrapping (`add_node` seam) and
the Keel-journal-backed checkpointer (design (a); see the module's own docs
for the full rationale).

LangGraph is NOT a repo dependency (adapter-pack rule 1: a pack never imports
its library unless present) and is not installed in this environment, so
every test here drives the pack against STRUCTURAL FAKES of the langgraph API
surface (`langgraph.graph.state.StateGraph`, `langgraph.checkpoint.base.
BaseCheckpointSaver`/`CheckpointTuple`) registered into `sys.modules` —
mirroring the shape documented in LangGraph's own README/source, never the
real package. The final class also verifies the checkpoint step-key
convention renders sensibly through the REAL `keel` CLI binary against a
hand-built fixture journal (skips cleanly if the binary is not built).
"""

from __future__ import annotations

import importlib.machinery
import json
import subprocess
import sqlite3
import sys
import types
import typing
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory
from typing import Any

from keel import _runtime
from keel._errors import KeelError
from keel.adapters._pack import Detection
from keel.packs import langgraph_pack
from keel.packs.tool import TARGET_ATTR

from . import CONTRACTS, REPO_ROOT


# --- structural fakes for the optional `langgraph` package -------------------


def _spec(name: str) -> importlib.machinery.ModuleSpec:
    return importlib.machinery.ModuleSpec(name, loader=None)


class _FakeRunnable:
    """Shaped like a LangChain `Runnable` used directly as a node: it has
    `.invoke()`, deliberately NOT `__call__` — `add_node` wrapping must skip
    it (module docs: "a bare Runnable node... passes through unwrapped")."""

    def invoke(self, state: Any) -> Any:
        return state


def install_fake_langgraph(version: str = "0.4.8") -> type:
    """Register a minimal `langgraph` package tree in `sys.modules` and
    return the fake `StateGraph` class. Mirrors the real add_node overloads
    (`add_node(fn)` / `add_node("name", fn)`) closely enough to exercise the
    pack's generic `inspect.signature`-based argument binding."""

    class FakeStateGraph:
        def __init__(self, schema: Any = None) -> None:
            self.nodes: dict[str, Any] = {}

        def add_node(
            self,
            node: Any,
            action: Any = None,
            *,
            metadata: dict[str, Any] | None = None,
            **kwargs: Any,
        ) -> "FakeStateGraph":
            if action is None:
                name = getattr(node, "name", None) or getattr(node, "__name__", None)
                self.nodes[name] = node
            else:
                self.nodes[node] = action
            return self

    graph_state = types.ModuleType("langgraph.graph.state")
    graph_state.StateGraph = FakeStateGraph  # type: ignore[attr-defined]
    graph_state.__spec__ = _spec("langgraph.graph.state")

    graph_pkg = types.ModuleType("langgraph.graph")
    graph_pkg.__path__ = []  # type: ignore[attr-defined]
    graph_pkg.StateGraph = FakeStateGraph  # type: ignore[attr-defined]
    graph_pkg.START = "__start__"  # type: ignore[attr-defined]
    graph_pkg.END = "__end__"  # type: ignore[attr-defined]
    graph_pkg.__spec__ = _spec("langgraph.graph")

    class FakeBaseCheckpointSaver:
        def __init__(self, *, serde: Any = None) -> None:
            self.serde = serde

    class FakeCheckpointTuple(typing.NamedTuple):
        config: dict[str, Any]
        checkpoint: dict[str, Any]
        metadata: dict[str, Any]
        parent_config: dict[str, Any] | None = None
        pending_writes: list[Any] | None = None

    checkpoint_base = types.ModuleType("langgraph.checkpoint.base")
    checkpoint_base.BaseCheckpointSaver = FakeBaseCheckpointSaver  # type: ignore[attr-defined]
    checkpoint_base.CheckpointTuple = FakeCheckpointTuple  # type: ignore[attr-defined]
    checkpoint_base.__spec__ = _spec("langgraph.checkpoint.base")

    checkpoint_pkg = types.ModuleType("langgraph.checkpoint")
    checkpoint_pkg.__path__ = []  # type: ignore[attr-defined]
    checkpoint_pkg.base = checkpoint_base  # type: ignore[attr-defined]
    checkpoint_pkg.__spec__ = _spec("langgraph.checkpoint")

    root = types.ModuleType("langgraph")
    root.__path__ = []  # type: ignore[attr-defined]
    root.__version__ = version  # type: ignore[attr-defined]
    root.graph = graph_pkg  # type: ignore[attr-defined]
    root.checkpoint = checkpoint_pkg  # type: ignore[attr-defined]
    root.__spec__ = _spec("langgraph")

    sys.modules.update(
        {
            "langgraph": root,
            "langgraph.graph": graph_pkg,
            "langgraph.graph.state": graph_state,
            "langgraph.checkpoint": checkpoint_pkg,
            "langgraph.checkpoint.base": checkpoint_base,
        }
    )
    return FakeStateGraph


def uninstall_fake_langgraph() -> None:
    for name in [n for n in sys.modules if n == "langgraph" or n.startswith("langgraph.")]:
        del sys.modules[name]


def _reset_pack_state() -> None:
    langgraph_pack._saver_cls = None
    langgraph_pack._installed = False
    langgraph_pack._orig_add_node = None


class _FakeExecuteBackend:
    """Records every `execute()` call and runs its effect immediately —
    exactly the shape `keel._flow`'s own `_FakeFlowBackend` uses (a flow's
    `execute` never sees a real network/IO call; only the effect's return
    value matters)."""

    def __init__(self) -> None:
        self.calls: list[dict[str, Any]] = []

    def execute(self, request: dict[str, Any], effect: Any) -> dict[str, Any]:
        self.calls.append(request)
        result = effect(0)
        return {"result": result.get("status", "ok"), "payload": result.get("payload")}

    def step_keys(self) -> list[str]:
        return [f"{c['target']}#{c['args_hash']}" for c in self.calls]


# --- contract shape -----------------------------------------------------------


class PackContractTest(unittest.TestCase):
    def tearDown(self) -> None:
        uninstall_fake_langgraph()

    def test_detect_absent(self) -> None:
        uninstall_fake_langgraph()
        det = langgraph_pack.detect()
        self.assertIsInstance(det, Detection)
        self.assertFalse(det.matched)

    def test_detect_present_pinned(self) -> None:
        install_fake_langgraph(version="0.4.8")
        det = langgraph_pack.detect()
        self.assertTrue(det.matched)
        self.assertEqual(det.name, "langgraph")

    def test_seams_names_add_node(self) -> None:
        seams = langgraph_pack.seams()
        self.assertEqual(len(seams), 1)
        self.assertIn("StateGraph.add_node", seams[0].patch_point)

    def test_targets_include_node_and_checkpoint_patterns(self) -> None:
        patterns = {t.pattern for t in langgraph_pack.targets()}
        self.assertIn("tool:<node-name>", patterns)
        self.assertIn(langgraph_pack.CHECKPOINT_PUT_TARGET, patterns)
        self.assertIn(langgraph_pack.CHECKPOINT_WRITE_TARGET, patterns)
        self.assertIn(langgraph_pack.CHECKPOINT_DELETE_TARGET, patterns)

    def test_defaults_empty(self) -> None:
        self.assertEqual(langgraph_pack.defaults(), {})


# --- node wrapping (the `add_node` seam) --------------------------------------


class AddNodePatchTest(unittest.TestCase):
    def setUp(self) -> None:
        _reset_pack_state()
        self.StateGraph = install_fake_langgraph()
        self.backend = _FakeExecuteBackend()
        _runtime.set_runtime(self.backend, None)

    def tearDown(self) -> None:
        langgraph_pack.uninstall()
        _reset_pack_state()
        uninstall_fake_langgraph()
        _runtime.clear_runtime()

    def test_install_noop_without_langgraph(self) -> None:
        uninstall_fake_langgraph()
        langgraph_pack.install()  # must not raise
        self.assertFalse(langgraph_pack._installed)

    def test_two_arg_form_wraps_as_tool_target(self) -> None:
        langgraph_pack.install()
        graph = self.StateGraph()

        def my_node(state: dict[str, Any]) -> dict[str, Any]:
            return {**state, "seen": True}

        graph.add_node("my_node", my_node)
        wrapped = graph.nodes["my_node"]
        self.assertEqual(getattr(wrapped, TARGET_ATTR), "tool:my_node")

        out = wrapped({"x": 1})
        self.assertEqual(out, {"x": 1, "seen": True})
        self.assertEqual(self.backend.calls[0]["target"], "tool:my_node")
        self.assertFalse(self.backend.calls[0]["idempotent"])

    def test_single_arg_form_infers_name_from_dunder_name(self) -> None:
        langgraph_pack.install()
        graph = self.StateGraph()

        def retrieve_docs(state: dict[str, Any]) -> dict[str, Any]:
            return state

        graph.add_node(retrieve_docs)
        wrapped = graph.nodes["retrieve_docs"]
        self.assertEqual(getattr(wrapped, TARGET_ATTR), "tool:retrieve_docs")

    def test_already_wrapped_callable_is_not_double_wrapped(self) -> None:
        langgraph_pack.install()
        graph = self.StateGraph()

        def node_fn(state: dict[str, Any]) -> dict[str, Any]:
            return state

        graph.add_node("node_fn", node_fn)
        once = graph.nodes["node_fn"]

        graph.add_node("node_fn", once)  # re-registering the ALREADY-wrapped callable
        twice = graph.nodes["node_fn"]
        self.assertIs(once, twice, "wrap_tool must not be applied a second time")

    def test_invalid_tool_name_left_unwrapped(self) -> None:
        langgraph_pack.install()
        graph = self.StateGraph()

        def node_fn(state: dict[str, Any]) -> dict[str, Any]:
            return state

        graph.add_node("bad name", node_fn)  # a space: not a valid tool: name
        self.assertIs(graph.nodes["bad name"], node_fn)
        self.assertIsNone(getattr(node_fn, TARGET_ATTR, None))

    def test_runnable_without_call_left_unwrapped(self) -> None:
        langgraph_pack.install()
        graph = self.StateGraph()
        runnable = _FakeRunnable()

        graph.add_node("chain", runnable)
        self.assertIs(graph.nodes["chain"], runnable)

    def test_uninstall_restores_original_add_node(self) -> None:
        original = self.StateGraph.add_node
        langgraph_pack.install()
        self.assertIsNot(self.StateGraph.add_node, original)
        langgraph_pack.uninstall()
        self.assertIs(self.StateGraph.add_node, original)


# --- the checkpointer ----------------------------------------------------------


class CheckpointerWithoutLangGraphTest(unittest.TestCase):
    def tearDown(self) -> None:
        uninstall_fake_langgraph()
        _reset_pack_state()

    def test_keel_saver_needs_langgraph_installed(self) -> None:
        uninstall_fake_langgraph()
        _reset_pack_state()
        with self.assertRaises(KeelError) as ctx:
            langgraph_pack.KeelSaver()
        self.assertEqual(ctx.exception.code, "KEEL-E005")


class CheckpointerHonestyGateTest(unittest.TestCase):
    def setUp(self) -> None:
        _reset_pack_state()
        install_fake_langgraph()

    def tearDown(self) -> None:
        _runtime.clear_runtime()
        uninstall_fake_langgraph()
        _reset_pack_state()

    def test_refuses_without_a_backend(self) -> None:
        saver = langgraph_pack.KeelSaver()
        with self.assertRaises(KeelError) as ctx:
            saver.put({"configurable": {"thread_id": "t1"}}, {"id": "c1"}, {}, {})
        self.assertEqual(ctx.exception.code, "KEEL-E005")

    def test_refuses_with_a_backend_but_no_active_flow(self) -> None:
        _runtime.set_runtime(_FakeExecuteBackend(), None)
        saver = langgraph_pack.KeelSaver()
        with self.assertRaises(KeelError) as ctx:
            saver.put({"configurable": {"thread_id": "t1"}}, {"id": "c1"}, {}, {})
        self.assertEqual(ctx.exception.code, "KEEL-E005")


def _checkpoint(cid: str, channels: dict[str, Any] | None = None) -> dict[str, Any]:
    return {
        "v": 4,
        "id": cid,
        "ts": "2026-07-13T00:00:00+00:00",
        "channel_values": channels or {},
        "channel_versions": {},
        "versions_seen": {},
    }


class CheckpointerFlowTest(unittest.TestCase):
    """Every op journals into (or reads back from) the CURRENTLY OPEN flow —
    exercised with `_runtime.set_flow_active(True)` and a fake flow-shaped
    backend, exactly as `_flow.run_as_flow` sets it up around the real flow
    body."""

    def setUp(self) -> None:
        _reset_pack_state()
        install_fake_langgraph()
        self.backend = _FakeExecuteBackend()
        _runtime.set_runtime(self.backend, None)
        _runtime.set_flow_active(True)
        self.saver = langgraph_pack.KeelSaver()

    def tearDown(self) -> None:
        _runtime.clear_runtime()
        uninstall_fake_langgraph()
        _reset_pack_state()

    def test_put_journals_the_documented_step_key_and_returns_config(self) -> None:
        config = {"configurable": {"thread_id": "thread-1", "checkpoint_ns": ""}}
        out = self.saver.put(config, _checkpoint("c1", {"x": 1}), {"step": 1}, {})
        self.assertEqual(
            out,
            {"configurable": {"thread_id": "thread-1", "checkpoint_ns": "", "checkpoint_id": "c1"}},
        )
        self.assertEqual(
            self.backend.step_keys(), [f"{langgraph_pack.CHECKPOINT_PUT_TARGET}#thread-1::1"]
        )
        self.assertFalse(self.backend.calls[0]["idempotent"])

    def test_put_seq_increments_per_thread_and_namespace(self) -> None:
        config = {"configurable": {"thread_id": "thread-1", "checkpoint_ns": ""}}
        self.saver.put(config, _checkpoint("c1"), {}, {})
        self.saver.put(config, _checkpoint("c2"), {}, {})
        other_ns = {"configurable": {"thread_id": "thread-1", "checkpoint_ns": "child:1"}}
        self.saver.put(other_ns, _checkpoint("c3"), {}, {})
        self.assertEqual(
            self.backend.step_keys(),
            [
                f"{langgraph_pack.CHECKPOINT_PUT_TARGET}#thread-1::1",
                f"{langgraph_pack.CHECKPOINT_PUT_TARGET}#thread-1::2",
                f"{langgraph_pack.CHECKPOINT_PUT_TARGET}#thread-1:child:1:1",
            ],
        )

    def test_get_tuple_returns_latest_when_no_checkpoint_id(self) -> None:
        config = {"configurable": {"thread_id": "thread-1", "checkpoint_ns": ""}}
        out1 = self.saver.put(config, _checkpoint("c1"), {"step": 1}, {})
        # A real caller threads the previous put()'s returned config into the
        # next put() (its checkpoint_id becomes the new checkpoint's parent).
        out2 = self.saver.put(out1, _checkpoint("c2"), {"step": 2}, {})
        self.assertEqual(out2["configurable"]["checkpoint_id"], "c2")

        tup = self.saver.get_tuple({"configurable": {"thread_id": "thread-1"}})
        self.assertEqual(tup.checkpoint["id"], "c2")
        self.assertEqual(
            tup.config,
            {"configurable": {"thread_id": "thread-1", "checkpoint_ns": "", "checkpoint_id": "c2"}},
        )
        self.assertEqual(
            tup.parent_config,
            {"configurable": {"thread_id": "thread-1", "checkpoint_ns": "", "checkpoint_id": "c1"}},
        )

    def test_get_tuple_by_explicit_checkpoint_id(self) -> None:
        config = {"configurable": {"thread_id": "thread-1", "checkpoint_ns": ""}}
        self.saver.put(config, _checkpoint("c1"), {}, {})
        self.saver.put(config, _checkpoint("c2"), {}, {})
        tup = self.saver.get_tuple(
            {"configurable": {"thread_id": "thread-1", "checkpoint_id": "c1"}}
        )
        self.assertEqual(tup.checkpoint["id"], "c1")

    def test_get_tuple_none_when_nothing_recorded(self) -> None:
        tup = self.saver.get_tuple({"configurable": {"thread_id": "brand-new-thread"}})
        self.assertIsNone(tup)

    def test_list_is_newest_first_and_respects_limit(self) -> None:
        config = {"configurable": {"thread_id": "thread-1", "checkpoint_ns": ""}}
        for cid in ("c1", "c2", "c3"):
            self.saver.put(config, _checkpoint(cid), {}, {})
        ids = [t.checkpoint["id"] for t in self.saver.list({"configurable": {"thread_id": "thread-1"}})]
        self.assertEqual(ids, ["c3", "c2", "c1"])
        limited = [
            t.checkpoint["id"]
            for t in self.saver.list({"configurable": {"thread_id": "thread-1"}}, limit=2)
        ]
        self.assertEqual(limited, ["c3", "c2"])

    def test_list_before_excludes_and_after(self) -> None:
        config = {"configurable": {"thread_id": "thread-1", "checkpoint_ns": ""}}
        for cid in ("c1", "c2", "c3"):
            self.saver.put(config, _checkpoint(cid), {}, {})
        before = {"configurable": {"thread_id": "thread-1", "checkpoint_id": "c3"}}
        ids = [
            t.checkpoint["id"]
            for t in self.saver.list({"configurable": {"thread_id": "thread-1"}}, before=before)
        ]
        self.assertEqual(ids, ["c2", "c1"])

    def test_list_filter_matches_metadata(self) -> None:
        config = {"configurable": {"thread_id": "thread-1", "checkpoint_ns": ""}}
        self.saver.put(config, _checkpoint("c1"), {"source": "loop"}, {})
        self.saver.put(config, _checkpoint("c2"), {"source": "input"}, {})
        ids = [
            t.checkpoint["id"]
            for t in self.saver.list(
                {"configurable": {"thread_id": "thread-1"}}, filter={"source": "input"}
            )
        ]
        self.assertEqual(ids, ["c2"])

    def test_put_writes_populates_pending_writes(self) -> None:
        config = {"configurable": {"thread_id": "thread-1", "checkpoint_ns": ""}}
        self.saver.put(config, _checkpoint("c1"), {}, {})
        write_config = {
            "configurable": {"thread_id": "thread-1", "checkpoint_ns": "", "checkpoint_id": "c1"}
        }
        self.saver.put_writes(write_config, [("channel_a", 1), ("channel_b", 2)], "task-1")
        tup = self.saver.get_tuple({"configurable": {"thread_id": "thread-1"}})
        self.assertEqual(
            tup.pending_writes, [("task-1", "channel_a", 1), ("task-1", "channel_b", 2)]
        )
        self.assertEqual(
            self.backend.step_keys()[-2:],
            [
                f"{langgraph_pack.CHECKPOINT_WRITE_TARGET}#thread-1::c1:task-1:0",
                f"{langgraph_pack.CHECKPOINT_WRITE_TARGET}#thread-1::c1:task-1:1",
            ],
        )

    def test_delete_thread_clears_state_and_journals(self) -> None:
        config = {"configurable": {"thread_id": "thread-1", "checkpoint_ns": ""}}
        self.saver.put(config, _checkpoint("c1"), {}, {})
        self.saver.delete_thread("thread-1")
        self.assertIsNone(self.saver.get_tuple({"configurable": {"thread_id": "thread-1"}}))
        self.assertEqual(self.backend.step_keys()[-1], f"{langgraph_pack.CHECKPOINT_DELETE_TARGET}#thread-1")

    def test_replay_reconstructs_identical_state(self) -> None:
        """The property the module docs lean on: a FRESH KeelSaver (as
        constructed after a `kill -9` resume, since the flow's entry function
        reruns from the top) that replays the SAME put() calls in the SAME
        order ends up in the SAME observable state — because put() never
        depends on `execute()`'s outcome, only on the live objects LangGraph
        already handed it (module docs)."""
        config = {"configurable": {"thread_id": "thread-1", "checkpoint_ns": ""}}
        checkpoints = [_checkpoint("c1", {"n": 1}), _checkpoint("c2", {"n": 2})]

        for cp in checkpoints:
            self.saver.put(config, cp, {"step": cp["channel_values"]["n"]}, {})
        before_ids = [t.checkpoint["id"] for t in self.saver.list({"configurable": {"thread_id": "thread-1"}})]

        # A brand-new saver instance (module docs: "a fresh KeelSaver()
        # object... starts with empty in-memory maps"), replaying the exact
        # same calls the crashed run made.
        resumed = langgraph_pack.KeelSaver()
        for cp in checkpoints:
            resumed.put(config, cp, {"step": cp["channel_values"]["n"]}, {})
        after_ids = [t.checkpoint["id"] for t in resumed.list({"configurable": {"thread_id": "thread-1"}})]

        self.assertEqual(before_ids, after_ids)
        self.assertEqual(
            resumed.get_tuple({"configurable": {"thread_id": "thread-1"}}).checkpoint,
            self.saver.get_tuple({"configurable": {"thread_id": "thread-1"}}).checkpoint,
        )


# --- CLI verification: the step-key convention renders sensibly --------------

_LANGGRAPH_FIXTURE_SQL = """
INSERT INTO flows (flow_id, entrypoint, args_hash, code_hash, status,
                   lease_holder, lease_expires, created_at, updated_at)
VALUES ('01JZWY0B0000000000000099', 'py:agent:run_graph',
        'ah-lg01', 'ch-lg01', 'completed',
        NULL, NULL, 1783728000000, 1783728006000);

INSERT INTO steps VALUES ('01JZWY0B0000000000000099', 1,
        'tool:retrieve_docs#-', 'effect', 1, 'ok',
        X'C0', NULL, 1783728000000, 1783728000400);

INSERT INTO steps VALUES ('01JZWY0B0000000000000099', 2,
        'tool:langgraph.checkpoint#thread-42::1', 'effect', 1, 'ok',
        X'C0', NULL, 1783728000401, 1783728000410);

INSERT INTO steps VALUES ('01JZWY0B0000000000000099', 3,
        'tool:generate_answer#-', 'effect', 1, 'ok',
        X'C0', NULL, 1783728000420, 1783728002100);

INSERT INTO steps VALUES ('01JZWY0B0000000000000099', 4,
        'tool:langgraph.checkpoint_write#thread-42::chk-2:task-1:0', 'effect', 1, 'ok',
        X'C0', NULL, 1783728002101, 1783728002110);

INSERT INTO steps VALUES ('01JZWY0B0000000000000099', 5,
        'tool:langgraph.checkpoint#thread-42::2', 'effect', 1, 'ok',
        X'C0', NULL, 1783728002111, 1783728002120);
"""


def _keel_binary() -> str | None:
    for candidate in (REPO_ROOT / "target" / "debug" / "keel", REPO_ROOT / "target" / "release" / "keel"):
        if candidate.exists():
            return str(candidate)
    return None


@unittest.skipUnless(_keel_binary(), "keel CLI binary not built (cargo build -p keel-cli)")
class CliRendersCheckpointStepsTest(unittest.TestCase):
    """`keel trace`/`keel flows` need no change to show LangGraph checkpoint
    steps sensibly (module docs): the step-key convention is ordinary
    `target#args_hash` content, rendered exactly like every other step."""

    def test_trace_shows_node_and_checkpoint_steps_in_call_order(self) -> None:
        with TemporaryDirectory() as tmp:
            journal_dir = Path(tmp) / ".keel"
            journal_dir.mkdir()
            con = sqlite3.connect(journal_dir / "journal.db")
            try:
                con.executescript((CONTRACTS / "journal.sql").read_text())
                con.executescript(_LANGGRAPH_FIXTURE_SQL)
                con.commit()
            finally:
                con.close()

            out = subprocess.run(
                [_keel_binary(), "trace", "01JZWY0B0000000000000099", "--json"],
                cwd=tmp,
                capture_output=True,
                text=True,
                check=True,
            )
            report = json.loads(out.stdout)
            step_keys = [s["step_key"] for s in report["steps"]]
            self.assertEqual(
                step_keys,
                [
                    "tool:retrieve_docs#-",
                    "tool:langgraph.checkpoint#thread-42::1",
                    "tool:generate_answer#-",
                    "tool:langgraph.checkpoint_write#thread-42::chk-2:task-1:0",
                    "tool:langgraph.checkpoint#thread-42::2",
                ],
            )


if __name__ == "__main__":
    unittest.main()
