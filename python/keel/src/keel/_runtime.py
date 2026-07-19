"""Process-global runtime state shared between the import hook's wrappers and
the bootstrap. Mirrors the Node front end's `runtime.mjs`: the generated
wrappers run wherever the user's code runs, so they reach the configured
backend + discovery store through this module rather than a captured closure.

When Keel is disabled (or never installed) `get_backend()` is None and every
wrapper falls through to the original function unchanged.
"""

from __future__ import annotations

from typing import TYPE_CHECKING, Iterable

if TYPE_CHECKING:
    from ._backend import Backend
    from ._discovery import Discovery
    from ._policy import FlowEntrypoint

_backend: "Backend | None" = None
_discovery: "Discovery | None" = None
#: Set only for the duration of a Tier 2 flow's body (`_flow.run_as_flow`), so
#: framework packs that persist through the flow journal (e.g. the LangGraph
#: checkpointer, `packs.langgraph_pack.KeelSaver`) can tell "a durable flow is
#: open on this backend" apart from "a native backend exists" — `execute()`
#: silently downgrades to a plain (unjournaled) Tier 1 call outside a flow, so
#: a pack that needs journaled durability must refuse rather than guess.
_flow_active: bool = False
#: The `[flows] entrypoints` parsed by `bootstrap.install_keel()`
#: (`_policy.extract_flow_entrypoints`), set once at install time. Framework
#: packs that need to check whether a specific call site is a designated
#: Tier 2 entrypoint (e.g. `packs.adk_pack._flow_entrypoint_designated`) read
#: this instead of reaching into `bootstrap`'s module-private state.
_flow_entrypoints: "tuple[FlowEntrypoint, ...]" = ()


def set_runtime(backend: "Backend | None", discovery: "Discovery | None") -> None:
    global _backend, _discovery
    _backend = backend
    _discovery = discovery


def clear_runtime() -> None:
    """Reset to the disabled state (used by `uninstall_keel` and tests)."""
    global _backend, _discovery, _flow_active, _flow_entrypoints
    _backend = None
    _discovery = None
    _flow_active = False
    _flow_entrypoints = ()


def get_backend() -> "Backend | None":
    return _backend


def get_discovery() -> "Discovery | None":
    return _discovery


def set_flow_active(active: bool) -> None:
    """Flip the "a Tier 2 flow body is currently running" flag. Called only by
    `_flow.run_as_flow` around the flow function's execution."""
    global _flow_active
    _flow_active = active


def in_active_flow() -> bool:
    """Whether a Tier 2 flow's body is currently executing on this backend —
    i.e. `execute()` calls route through the flow's journaled `execute_step`
    rather than the bare (unjournaled) engine."""
    return _flow_active


def set_flow_entrypoints(entrypoints: "Iterable[FlowEntrypoint]") -> None:
    """Store the `[flows] entrypoints` parsed at install time. Called only by
    `bootstrap.install_keel()`, once, with `_policy.extract_flow_entrypoints`'s
    result."""
    global _flow_entrypoints
    _flow_entrypoints = tuple(entrypoints)


def get_flow_entrypoints() -> "tuple[FlowEntrypoint, ...]":
    """The `[flows] entrypoints` parsed at install time, or `()` when Keel was
    never installed (or `KEEL_DISABLE` short-circuited before installing) —
    exactly like `get_backend()` returning `None` in that same situation."""
    return _flow_entrypoints
