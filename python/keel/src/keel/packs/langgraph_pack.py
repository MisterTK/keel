"""LangGraph: node-execution wrapping + the Keel-journal-backed checkpointer
(dx-spec §4.2, sprint-plan Team E line 42 + standing structure item 2).

Two independent, optional pieces — LangGraph absent means zero effect, per
adapter-pack rule 1 (this module never imports ``langgraph`` at import time):

1. **Node wrapping** (``detect``/``seams``/``targets``/``defaults`` +
   ``install``/``uninstall``, wired into ``keel.adapters`` exactly like the
   httpx/requests library packs — a real, monkey-patched seam, not a semantic
   pack like ``llm``/``tool``): patches ``StateGraph.add_node`` so every
   node's callable becomes a ``tool:<node-name>`` policy target *before*
   LangGraph's own Pregel engine ever holds a reference to it. A failed node
   then retries below the graph step (dx-spec §4.2's "call-level resilience
   under the graph") without a code change to the node function.

   Seam choice: ``StateGraph.add_node`` — LangGraph's public, documented
   node-registration API (``libs/langgraph/langgraph/graph/state.py``) — NOT
   Pregel's internal node-invocation machinery (``PregelNode``/
   ``RunnableSeq``/``RunnableCallable``), which is undocumented and has moved
   across releases. Wrapping at registration means the invocation path never
   needs touching: whatever Pregel does with the node later, it does it to
   OUR wrapper. Only a plain callable is wrapped — a LangChain ``Runnable``
   passed directly as a node (composed with ``|``, no ``__call__``) is left
   untouched; its own model/tool calls still ride the ``llm:``/``tool:``
   seams (httpx transport, explicit ``tool.wrap_tool`` call sites), so
   wrapping the node boundary too is additive, never a double-wrap of the
   SAME call — model/tool calls happen *inside* a node's execution, the
   node-boundary wrap is *around* it.

2. **`KeelSaver`** — a ``BaseCheckpointSaver`` that journals LangGraph
   checkpoints as steps of the CURRENTLY OPEN Keel Tier 2 durable flow, so
   graph-state persistence and Keel's own effect journal share one file and
   one ``keel trace`` view (the "strategic move", dx-spec §4.2).

   Design (a) — the ONLY design considered (a CCR-requiring design (b), a
   dedicated checkpoint table in ``contracts/journal.sql``, was rejected: no
   contract change is in scope for this pack): checkpoints/writes are
   ordinary journaled flow STEPS through the EXISTING flow API
   (``backend.execute`` while a flow is open — the same call every wrapped
   ``py:``/``tool:``/``llm:`` call already makes), not new journal rows or
   FFI surface.

   Step-key convention (frozen for THIS PACK, not a core contract — ordinary
   ``args_hash`` content, chosen exactly like every other framework pack's):
   ``target`` is one of the three constants below; ``args_hash`` is
   ``"<thread_id>:<checkpoint_ns>:<seq>"`` for a checkpoint put (``seq`` is a
   1-based, per-thread-per-namespace counter of puts made so far *in this
   flow*) and ``"<thread_id>:<checkpoint_ns>:<checkpoint_id>:<task_id>:<idx>"``
   for a pending write. Since ``steps.step_key`` is ``"<target>#<args_hash>"``
   (``keel-core/src/flow.rs::step_key``), a put renders in ``keel trace``/
   ``keel flows`` as e.g. ``tool:langgraph.checkpoint#thread-42::3`` — legible
   without any CLI change (verified against a hand-built fixture journal;
   see ``tests/test_packs_langgraph.py``).

   Why this is correct under replay: a Tier 2 flow resume re-invokes the
   flow's function from the top (``_flow.run_as_flow``/``keel-core::flow``
   module docs) — the WHOLE graph re-runs, not just the crashed node. Every
   node call is itself a journaled ``tool:<node-name>`` step (piece 1 above),
   so replay substitutes each already-completed node's result without
   re-firing its side effects; ``KeelSaver`` puts interleave with those node
   steps in the SAME flow-wide ``seq`` order, so they replay/live-execute in
   lockstep exactly like any other step. A fresh ``KeelSaver()`` object
   (reconstructed on resume, since the flow's entry function reruns from
   scratch) starts with empty in-memory maps; replaying the flow's earlier
   ``put()`` calls in the SAME order rebuilds an equivalent checkpoint chain
   as it goes — verified against the REAL package (a throwaway venv, never a
   repo dependency): the recovered ``channel_values`` converge to EXACTLY the
   pre-crash values (no node's side effect ever re-fires), while each
   ``checkpoint["id"]`` is LangGraph's OWN fresh ``uuid6`` allocation made
   fresh on every ``invoke()`` — those are NOT expected to match the
   pre-crash run's ids (nothing virtualizes them; they are bookkeeping
   identifiers, not observable graph state) and this pack never assumes they
   do. By the time live execution resumes past the crash point, the
   in-memory checkpoint chain is self-consistent and holds the correct
   values, under a fresh set of ids.

   Documented scope limit (why this is design (a) done HONESTLY, not
   half-done): ``get_tuple``/``list`` answer from THIS in-memory state only —
   built from THIS flow's own replayed/live ``put()`` calls. They do **not**
   reach across flow_ids to read a DIFFERENT flow's history (e.g. "resume
   this thread_id in a brand-new `keel run` invocation, a separate flow").
   That would require reading historical ``steps.payload`` bytes directly out
   of ``.keel/journal.db`` — which are real MessagePack (`rmp_serde`,
   `keel-core/src/flow.rs::encode_payload`) — and this pack's territory
   (`python/keel`, stdlib-only, no new manifest dependency) has no msgpack
   decoder to read them with; adding one, or a tiny read-only FFI/query
   surface on `keel_core`, is future work (a real, concrete blocker, not
   scope-creep avoidance). What IS fully solved is the dx-spec's headline
   promise for this slice — kill-9 mid-graph, then resume the SAME flow and
   get the SAME checkpoints back — because that never leaves the current
   flow_id. A checkpointer used outside an open flow refuses loudly
   (KEEL-E005) rather than silently downgrading to ephemeral, non-durable
   state (dx-spec: never a silent Tier-1-shaped surprise).
"""

from __future__ import annotations

import functools
import inspect
from dataclasses import dataclass, field
from typing import Any, Callable, Iterator

from .. import _runtime
from .._errors import KeelError
from .._wrap import ENVELOPE_VERSION, _json_safe
from ..adapters._pack import Detection, Seam, TargetDecl
from ._provider import detect_pack
from .tool import is_valid_tool_name, wrap_tool
from .tool import TARGET_ATTR as _TOOL_TARGET_ATTR

MODULE = "langgraph"
NAME = "langgraph"

#: Versions this pack certifies via the fixture/contract tests below (prefix
#: match against ``importlib.metadata.version``). Outside the range `detect`
#: reports `best_effort` — the `add_node` seam still tries (it has been
#: stable since 0.2).
_PINNED = ("0.2", "0.3", "0.4", "0.5", "0.6", "1.0")

#: The checkpoint-pack step targets (module docs). All three share the
#: `tool:` namespace (the frozen targetKey grammar admits no other prefix for
#: a framework-pack-owned call boundary; contracts/policy.schema.json).
CHECKPOINT_PUT_TARGET = "tool:langgraph.checkpoint"
CHECKPOINT_WRITE_TARGET = "tool:langgraph.checkpoint_write"
CHECKPOINT_DELETE_TARGET = "tool:langgraph.checkpoint_delete"


# --- contract operations (node-wrapping pack) --------------------------------


def detect() -> Detection:
    """Present iff ``langgraph`` is importable — decided without importing it
    (adapter-pack rule 1)."""
    return detect_pack(MODULE, NAME, _PINNED)


def seams() -> list[Seam]:
    return [
        Seam(
            patch_point="langgraph.graph.state.StateGraph.add_node",
            upstream_api=(
                "LangGraph graph-building API: "
                "StateGraph.add_node(node, action=None, **kwargs) -> Self"
            ),
            why_stable=(
                "the public, documented entry point every LangGraph program calls "
                "to register a node's callable; wrapping here means the node "
                "always arrives at Pregel's (internal, version-churning) "
                "invocation machinery already wrapped, with no need to patch "
                "that machinery directly. Only a plain callable action is "
                "wrapped — a bare Runnable node (no __call__) passes through "
                "unwrapped; its own llm:/tool: seams still apply."
            ),
        )
    ]


def targets() -> list[TargetDecl]:
    node = TargetDecl(
        pattern="tool:<node-name>",
        kind="tool",
        idempotency_rule=(
            "a graph node call is non-idempotent by default — observed, not "
            "retried (KEEL-E014) — the same Level 0 rule tool.wrap_tool "
            "applies everywhere else; a node that IS safe to retry must be "
            "wrapped explicitly with wrap_tool(idempotent=True) by its own "
            "code, not by this pack's automatic add_node patch"
        ),
        args_hash_rule="None — a node call is never cached (non-idempotent by default)",
    )
    checkpoint = TargetDecl(
        pattern=CHECKPOINT_PUT_TARGET,
        kind="tool",
        idempotency_rule="a checkpoint put is a non-idempotent bookkeeping write; never retried",
        args_hash_rule=(
            "'<thread_id>:<checkpoint_ns>:<seq>' — the checkpoint step-key "
            "convention (module docs), NOT a cache key (no caching applies)"
        ),
    )
    checkpoint_write = TargetDecl(
        pattern=CHECKPOINT_WRITE_TARGET,
        kind="tool",
        idempotency_rule="a pending-write record is a non-idempotent bookkeeping write; never retried",
        args_hash_rule="'<thread_id>:<checkpoint_ns>:<checkpoint_id>:<task_id>:<idx>'",
    )
    checkpoint_delete = TargetDecl(
        pattern=CHECKPOINT_DELETE_TARGET,
        kind="tool",
        idempotency_rule="a thread deletion is a non-idempotent bookkeeping write; never retried",
        args_hash_rule="'<thread_id>'",
    )
    return [node, checkpoint, checkpoint_write, checkpoint_delete]


def defaults() -> dict[str, Any]:
    """Empty: like `tool:`, every target here inherits `[defaults.outbound]`
    through the backend's target resolution — this pack ships no fragment of
    its own."""
    return {}


# --- install / uninstall (node-wrapping pack) --------------------------------

_installed = False
_orig_add_node: Callable[..., Any] | None = None


def install() -> None:
    """Patch `StateGraph.add_node`. Idempotent; a no-op if langgraph is not
    importable."""
    global _installed, _orig_add_node
    if _installed:
        return
    try:
        from langgraph.graph.state import StateGraph
    except ImportError:
        return
    _orig_add_node = StateGraph.add_node
    StateGraph.add_node = _make_add_node_wrapper(_orig_add_node)  # type: ignore[method-assign]
    _installed = True


def uninstall() -> None:
    """Restore the original `StateGraph.add_node`."""
    global _installed
    if not _installed:
        return
    try:
        from langgraph.graph.state import StateGraph
    except ImportError:
        _installed = False
        return
    if _orig_add_node is not None:
        StateGraph.add_node = _orig_add_node  # type: ignore[method-assign]
    _installed = False


def _make_add_node_wrapper(orig: Callable[..., Any]) -> Callable[..., Any]:
    """Wrap `StateGraph.add_node`, binding whatever calling convention the
    caller used (positional or keyword `node`/`action`) via the ORIGINAL
    method's own signature, so both the `add_node(fn)` and
    `add_node("name", fn)` overloads (and any future keyword-only addition)
    are handled without guessing argument order."""
    sig = inspect.signature(orig)

    @functools.wraps(orig)
    def add_node(self: Any, *args: Any, **kwargs: Any) -> Any:
        try:
            bound = sig.bind(self, *args, **kwargs)
        except TypeError:
            return orig(self, *args, **kwargs)  # unrecognized shape: don't guess
        bound.apply_defaults()
        node = bound.arguments.get("node")
        action = bound.arguments.get("action")
        new_node, new_action = _prepare_node(node, action)
        bound.arguments["node"] = new_node
        if "action" in bound.arguments:
            bound.arguments["action"] = new_action
        return orig(*bound.args, **bound.kwargs)

    add_node.__keel_wrapped__ = True  # type: ignore[attr-defined]
    return add_node


def _prepare_node(node: Any, action: Any) -> tuple[Any, Any]:
    """Wrap the node's callable as `tool:<node-name>` before LangGraph ever
    sees it. Anything unsafe to name or wrap (a non-callable action, a
    Runnable with no `__call__`, an already-wrapped callable, an invalid
    tool: name) passes through UNCHANGED — a dx-honest skip, never a crash
    mid-bootstrap (mirrors `tool.is_valid_tool_name`'s own doc contract)."""
    if action is None:
        # single-callable form: `node` IS the action; LangGraph infers its
        # registered name from it the same way we do here.
        name = _infer_node_name(node)
        return _maybe_wrap(name, node), None
    if isinstance(node, str):
        return node, _maybe_wrap(node, action)
    return node, action


def _infer_node_name(action: Any) -> str | None:
    name = getattr(action, "name", None)
    if isinstance(name, str) and name:
        return name
    inferred = getattr(action, "__name__", None)
    return inferred if isinstance(inferred, str) else None


def _maybe_wrap(name: Any, fn: Any) -> Any:
    if not isinstance(name, str) or not callable(fn):
        return fn  # can't name it, or it's a Runnable (no __call__): leave it
    if getattr(fn, _TOOL_TARGET_ATTR, None) is not None:
        return fn  # already Keel-wrapped (re-registration / recompile): no double-wrap
    if not is_valid_tool_name(name):
        return fn  # not a valid tool: name (e.g. a reserved "__…__" name): leave it
    return wrap_tool(name, fn)


# --- the Keel-journal-backed checkpointer (design (a); module docs) ---------


def _require_flow_backend() -> Any:
    """The honesty gate every `KeelSaver` operation checks first: `execute()`
    silently downgrades to a plain, unjournaled Tier 1 call outside an open
    flow (`_runtime.in_active_flow` docs) — never a place to keep a
    checkpoint. Refuse precisely (KEEL-E005, unsupported-configuration:
    valid usage, missing capability) rather than silently losing durability."""
    backend = _runtime.get_backend()
    if backend is None or not _runtime.in_active_flow():
        raise KeelError(
            "KEEL-E005",
            "KeelSaver needs an OPEN Keel Tier 2 durable flow: it persists by "
            "journaling into the CURRENTLY RUNNING flow's steps (dx-spec §4.2, "
            "\"one file, one trace view\"), and outside a flow `execute()` runs "
            "un-journaled — never a place to keep a checkpoint.\n"
            "  next: designate the graph's entrypoint under `[flows] "
            "entrypoints` (py:module:function) and run it via `keel run`, or "
            "use a different checkpointer (e.g. "
            "langgraph.checkpoint.memory.InMemorySaver) for a non-durable graph.",
        )
    return backend


def _checkpoint_step_key(thread_id: str, checkpoint_ns: str, seq: int) -> str:
    """The checkpoint-pack's step-key `args_hash` convention (module docs)."""
    return f"{thread_id}:{checkpoint_ns}:{seq}"


def _record_step(backend: Any, target: str, op: str, args_hash: str, payload: dict[str, Any]) -> None:
    """Journal one checkpoint-pack step. The payload is ALREADY fully known —
    LangGraph hands us a complete checkpoint/write to persist, there is
    nothing left to compute — so the effect never fails and its outcome is
    never read back: durability + `keel trace` visibility are the only
    reasons to journal at all (see the class docs for why correctness never
    depends on reading this payload back)."""
    request = {
        "v": ENVELOPE_VERSION,
        "target": target,
        "op": op,
        "idempotent": False,
        "args_hash": args_hash,
    }
    backend.execute(request, lambda _attempt: {"status": "ok", "payload": payload})


@dataclass
class _Entry:
    """One journaled checkpoint, held live (module docs: correctness never
    depends on round-tripping the journaled JSON summary back into this)."""

    checkpoint_id: str
    checkpoint_ns: str
    parent_checkpoint_id: str | None
    checkpoint: Any
    metadata: dict[str, Any] = field(default_factory=dict)


_saver_cls: type | None = None


def _base_checkpoint_saver_cls() -> type:
    """Build (once) and return the `BaseCheckpointSaver` subclass. A function,
    not a module-level `class` statement, so `langgraph` is imported only
    when a caller actually asks for a checkpointer (adapter-pack rule 1) —
    this module has zero effect in a program that never uses LangGraph."""
    global _saver_cls
    if _saver_cls is not None:
        return _saver_cls
    try:
        from langgraph.checkpoint.base import BaseCheckpointSaver, CheckpointTuple
    except ImportError as exc:
        raise KeelError(
            "KEEL-E005",
            "KeelSaver needs the `langgraph` package installed (it implements "
            "langgraph.checkpoint.base.BaseCheckpointSaver); install langgraph "
            "or pass a different checkpointer to your graph's .compile()",
        ) from exc

    class _KeelSaver(BaseCheckpointSaver):  # type: ignore[misc,valid-type]
        """`BaseCheckpointSaver` backed by the currently open Keel Tier 2 flow
        (module docs: design (a), the step-key convention, and the documented
        cross-flow scope limit)."""

        def __init__(self, *, serde: Any = None) -> None:
            super().__init__(serde=serde)
            # thread_id -> checkpoint_ns -> ordered (oldest-first) entries.
            self._by_ns: dict[str, dict[str, list[_Entry]]] = {}
            self._by_id: dict[tuple[str, str, str], _Entry] = {}
            self._writes: dict[tuple[str, str, str], list[tuple[str, str, Any]]] = {}

        # -- writes: journaled through the active flow ----------------------

        def put(
            self,
            config: dict[str, Any],
            checkpoint: dict[str, Any],
            metadata: dict[str, Any],
            new_versions: Any,
        ) -> dict[str, Any]:
            backend = _require_flow_backend()
            configurable = config["configurable"]
            thread_id = str(configurable["thread_id"])
            checkpoint_ns = configurable.get("checkpoint_ns", "")
            parent_id = configurable.get("checkpoint_id")
            checkpoint_id = checkpoint["id"]
            meta = dict(metadata) if isinstance(metadata, dict) else {}

            ns_list = self._by_ns.setdefault(thread_id, {}).setdefault(checkpoint_ns, [])
            seq = len(ns_list) + 1
            channels = sorted(checkpoint.get("channel_values", {}).keys())
            _record_step(
                backend,
                CHECKPOINT_PUT_TARGET,
                f"langgraph checkpoint put thread={thread_id} seq={seq}",
                _checkpoint_step_key(thread_id, checkpoint_ns, seq),
                {
                    "checkpoint_id": checkpoint_id,
                    "parent_checkpoint_id": parent_id,
                    "checkpoint_ns": checkpoint_ns,
                    "step": meta.get("step") if isinstance(meta.get("step"), int) else None,
                    "channels": _json_safe(channels) or [],
                },
            )
            entry = _Entry(
                checkpoint_id=checkpoint_id,
                checkpoint_ns=checkpoint_ns,
                parent_checkpoint_id=parent_id,
                checkpoint=checkpoint,
                metadata=meta,
            )
            ns_list.append(entry)
            self._by_id[(thread_id, checkpoint_ns, checkpoint_id)] = entry
            return {
                "configurable": {
                    "thread_id": thread_id,
                    "checkpoint_ns": checkpoint_ns,
                    "checkpoint_id": checkpoint_id,
                }
            }

        def put_writes(
            self,
            config: dict[str, Any],
            writes: Any,
            task_id: str,
            task_path: str = "",
        ) -> None:
            backend = _require_flow_backend()
            configurable = config["configurable"]
            thread_id = str(configurable["thread_id"])
            checkpoint_ns = configurable.get("checkpoint_ns", "")
            checkpoint_id = configurable["checkpoint_id"]
            key = (thread_id, checkpoint_ns, checkpoint_id)
            pending = self._writes.setdefault(key, [])
            for channel, value in writes:
                idx = len(pending)
                pending.append((task_id, channel, value))
                _record_step(
                    backend,
                    CHECKPOINT_WRITE_TARGET,
                    f"langgraph checkpoint_write thread={thread_id} task={task_id}",
                    f"{thread_id}:{checkpoint_ns}:{checkpoint_id}:{task_id}:{idx}",
                    {
                        "checkpoint_id": checkpoint_id,
                        "task_id": task_id,
                        "task_path": task_path,
                        "channel": channel,
                        "value": _json_safe(value),
                    },
                )

        def delete_thread(self, thread_id: str) -> None:
            backend = _require_flow_backend()
            thread_id = str(thread_id)
            _record_step(
                backend,
                CHECKPOINT_DELETE_TARGET,
                f"langgraph checkpoint delete thread={thread_id}",
                thread_id,
                {"thread_id": thread_id},
            )
            namespaces = self._by_ns.pop(thread_id, {})
            for checkpoint_ns, entries in namespaces.items():
                for entry in entries:
                    self._by_id.pop((thread_id, checkpoint_ns, entry.checkpoint_id), None)
                    self._writes.pop((thread_id, checkpoint_ns, entry.checkpoint_id), None)

        # -- reads: pure in-memory, never journaled (class docs) ------------

        def get_tuple(self, config: dict[str, Any]) -> Any:
            _require_flow_backend()
            configurable = config["configurable"]
            thread_id = str(configurable["thread_id"])
            checkpoint_ns = configurable.get("checkpoint_ns", "")
            checkpoint_id = configurable.get("checkpoint_id")
            if checkpoint_id is not None:
                entry = self._by_id.get((thread_id, checkpoint_ns, checkpoint_id))
            else:
                ns_list = self._by_ns.get(thread_id, {}).get(checkpoint_ns, [])
                entry = ns_list[-1] if ns_list else None
            if entry is None:
                return None
            return self._to_tuple(CheckpointTuple, thread_id, entry)

        def list(
            self,
            config: dict[str, Any] | None,
            *,
            filter: dict[str, Any] | None = None,
            before: dict[str, Any] | None = None,
            limit: int | None = None,
        ) -> Iterator[Any]:
            _require_flow_backend()
            configurable = config.get("configurable", {}) if config else {}
            thread_ids = [str(configurable["thread_id"])] if "thread_id" in configurable else list(
                self._by_ns.keys()
            )
            checkpoint_ns = configurable.get("checkpoint_ns") if "checkpoint_ns" in configurable else None
            before_id = None
            if before is not None:
                before_id = before.get("configurable", {}).get("checkpoint_id")
            count = 0
            for thread_id in thread_ids:
                namespaces = self._by_ns.get(thread_id, {})
                ns_keys = [checkpoint_ns] if checkpoint_ns is not None else list(namespaces.keys())
                for ns in ns_keys:
                    for entry in reversed(namespaces.get(ns, [])):
                        if before_id is not None and entry.checkpoint_id >= before_id:
                            continue
                        if filter and not all(entry.metadata.get(k) == v for k, v in filter.items()):
                            continue
                        yield self._to_tuple(CheckpointTuple, thread_id, entry)
                        count += 1
                        if limit is not None and count >= limit:
                            return

        def _to_tuple(self, tuple_cls: type, thread_id: str, entry: _Entry) -> Any:
            config = {
                "configurable": {
                    "thread_id": thread_id,
                    "checkpoint_ns": entry.checkpoint_ns,
                    "checkpoint_id": entry.checkpoint_id,
                }
            }
            parent_config = None
            if entry.parent_checkpoint_id is not None:
                parent_config = {
                    "configurable": {
                        "thread_id": thread_id,
                        "checkpoint_ns": entry.checkpoint_ns,
                        "checkpoint_id": entry.parent_checkpoint_id,
                    }
                }
            pending = self._writes.get((thread_id, entry.checkpoint_ns, entry.checkpoint_id))
            return tuple_cls(
                config=config,
                checkpoint=entry.checkpoint,
                metadata=entry.metadata,
                parent_config=parent_config,
                pending_writes=list(pending) if pending else None,
            )

    _saver_cls = _KeelSaver
    return _saver_cls


def KeelSaver(*args: Any, **kwargs: Any) -> Any:
    """Factory returning a `BaseCheckpointSaver` instance whose writes are
    journaled as steps of the CURRENTLY OPEN Keel Tier 2 flow (module docs).
    A callable, not a `class` statement, so `langgraph` is imported only when
    this is actually called (adapter-pack rule 1) — used exactly like a
    constructor: ``checkpointer = KeelSaver()``.
    """
    return _base_checkpoint_saver_cls()(*args, **kwargs)


__all__ = [
    "MODULE",
    "NAME",
    "CHECKPOINT_PUT_TARGET",
    "CHECKPOINT_WRITE_TARGET",
    "CHECKPOINT_DELETE_TARGET",
    "detect",
    "seams",
    "targets",
    "defaults",
    "install",
    "uninstall",
    "KeelSaver",
]
