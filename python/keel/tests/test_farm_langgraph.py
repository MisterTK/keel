"""Farm contract test: keel.packs.langgraph_pack against the REAL langgraph.

Runs ONLY under KEEL_ADAPTER_FARM=1 (see test_farm_adk.py's module docs for
the full convention). The offline fast path is tests/test_packs_langgraph.py
against structural fakes. This module certifies, on the real package
(langgraph 1.0.10 — the latest 1.0.x release at certification time; see
ws3-task-1-report.md for the exact pin Task 2's matrix freezes):

* ``StateGraph.add_node`` binds BOTH overloads
  (``add_node(fn)`` / ``add_node("name", fn)``) via
  ``inspect.signature(...).bind`` against the REAL unbound method
  (langgraph_pack.py:141-143's documented seam);
* ``langgraph.checkpoint.base.BaseCheckpointSaver`` and ``CheckpointTuple``
  import cleanly (langgraph_pack.py:370's ``_base_checkpoint_saver_cls``);
* ``keel_saver()`` — the pack's ``KeelSaver`` factory — returns an instance
  whose ``put``/``put_writes``/``get_tuple``/``list``/``delete_thread``
  signatures bind the pack's documented call shapes (verified against the
  REAL ``BaseCheckpointSaver`` abstract method signatures);
* a real 2-node ``StateGraph`` with plain-callable nodes (both the
  single-callable ``add_node(fn)`` and the two-arg ``add_node("name", fn)``
  forms) compiles and runs with its nodes wrapped — discovery rows appear
  under ``tool:<node>``;
* ``KeelSaver`` used outside an open Keel Tier 2 flow raises KEEL-E005 (the
  documented refusal, langgraph_pack.py:302-321), even against the real
  ``BaseCheckpointSaver`` base class.

No adjustment to the pack's calls was needed against the real 1.0.10 API —
``StateGraph.add_node``'s signature and every ``BaseCheckpointSaver`` abstract
method matched the module's documented assumptions exactly.
"""

from __future__ import annotations

import inspect
import os
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory
from typing import TypedDict

FARM = os.environ.get("KEEL_ADAPTER_FARM") == "1"
SKIP = "KEEL_ADAPTER_FARM=1 not set (offline fast path — see test_packs_langgraph.py)"

if FARM:  # real imports only in farm mode — never at fast-path collection time
    from langgraph.checkpoint.base import BaseCheckpointSaver, CheckpointTuple
    from langgraph.graph import END, START, StateGraph

from keel import _runtime
from keel._backend import load_backend
from keel._discovery import Discovery
from keel._errors import KeelError
from keel.packs import langgraph_pack


class _State(TypedDict):
    x: int


def _node_a(state: "_State") -> dict:
    return {"x": state["x"] + 1}


def _node_b(state: "_State") -> dict:
    return {"x": state["x"] * 2}


@unittest.skipUnless(FARM, SKIP)
class LangGraphFarmContractTest(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = TemporaryDirectory()
        backend = load_backend("stub")
        backend.configure({"target": {"tool:_node_a": {}, "tool:_node_b": {}}})
        self.discovery = Discovery(Path(self._tmp.name))
        _runtime.set_runtime(backend, self.discovery)
        langgraph_pack.install()

    def tearDown(self) -> None:
        langgraph_pack.uninstall()
        _runtime.clear_runtime()
        self.discovery.close()
        self._tmp.cleanup()

    def test_detect_reports_pinned_on_the_installed_version(self) -> None:
        det = langgraph_pack.detect()
        self.assertTrue(det.matched)
        self.assertEqual(det.confidence, "pinned", f"real version {det.version} fell out of range")

    def test_add_node_signature_binds_both_overloads(self) -> None:
        sig = inspect.signature(StateGraph.add_node)
        single = sig.bind(object(), _node_a)
        single.apply_defaults()
        self.assertIsNone(single.arguments.get("action"))
        two_arg = sig.bind(object(), "node_a", _node_a)
        two_arg.apply_defaults()
        self.assertEqual(two_arg.arguments.get("node"), "node_a")
        self.assertTrue(callable(two_arg.arguments.get("action")))

    def test_checkpoint_base_imports_and_keel_saver_signatures_bind(self) -> None:
        saver = langgraph_pack.KeelSaver()
        self.assertIsInstance(saver, BaseCheckpointSaver)
        inspect.signature(saver.put).bind(
            {"configurable": {"thread_id": "t1"}}, {"id": "c1"}, {}, {}
        )
        inspect.signature(saver.put_writes).bind(
            {"configurable": {"thread_id": "t1"}}, [("chan", 1)], "task-1"
        )
        inspect.signature(saver.get_tuple).bind({"configurable": {"thread_id": "t1"}})
        inspect.signature(saver.list).bind({"configurable": {"thread_id": "t1"}})
        inspect.signature(saver.delete_thread).bind("t1")

    def test_two_node_real_graph_compiles_and_runs_with_nodes_wrapped(self) -> None:
        graph = StateGraph(_State)
        graph.add_node("_node_a", _node_a)
        graph.add_node("_node_b", _node_b)
        graph.add_edge(START, "_node_a")
        graph.add_edge("_node_a", "_node_b")
        graph.add_edge("_node_b", END)
        compiled = graph.compile()

        result = compiled.invoke({"x": 1})
        self.assertEqual(result, {"x": 4})  # (1 + 1) * 2

        stats = _runtime.get_backend().report()["targets"]
        self.assertEqual(stats["tool:_node_a"]["successes"], 1)
        self.assertEqual(stats["tool:_node_b"]["successes"], 1)

    def test_single_callable_add_node_form_infers_name_and_wraps(self) -> None:
        graph = StateGraph(_State)
        graph.add_node(_node_a)  # single-callable overload: name from __name__
        graph.add_edge(START, "_node_a")
        graph.add_edge("_node_a", END)
        compiled = graph.compile()

        result = compiled.invoke({"x": 1})
        self.assertEqual(result, {"x": 2})
        self.assertEqual(_runtime.get_backend().report()["targets"]["tool:_node_a"]["successes"], 1)

    def test_keel_saver_outside_an_open_flow_refuses_with_e005(self) -> None:
        # A backend IS active (setUp), but no Tier 2 flow is open — the
        # documented honesty gate must refuse against the REAL
        # BaseCheckpointSaver base class exactly as it does against the fake.
        saver = langgraph_pack.KeelSaver()
        with self.assertRaises(KeelError) as ctx:
            saver.put({"configurable": {"thread_id": "t1"}}, {"id": "c1"}, {}, {})
        self.assertEqual(ctx.exception.code, "KEEL-E005")


if __name__ == "__main__":
    unittest.main()
