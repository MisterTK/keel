"""Bootstrap: everything `keel run` does before user code executes, in one
testable function.

Order matters: disable-check → policy → backend.configure → runtime state →
import hook (before the app + its deps import) → exit flush → banner. When
KEEL_DISABLE is set this returns immediately with zero effects, so a run is
byte-identical to one with no Keel at all (DX invariant / dx-spec §3).

Config errors (unreadable/invalid keel.toml, invalid policy) raise a KEEL-E001
and are intentionally fatal: a broken policy is a loud failure the user must
fix, not a silent fall-back to defaults (a Level 0 surprise is a P0).
"""

from __future__ import annotations

import atexit
import os
import sys
from pathlib import Path
from typing import Any, Mapping

from ._backend import load_backend
from ._defaults import apply_pack_defaults
from ._discovery import Discovery
from ._hook import KeelFinder, install_import_hook, remove_import_hook
from ._policy import (
    extract_cmd_flows,
    extract_flow_entrypoints,
    extract_function_targets,
    load_policy,
)
from ._runtime import (
    clear_runtime,
    get_backend,
    set_cmd_flows,
    set_flow_entrypoints,
    set_runtime,
)
from ._targets import clear_outbound_targets, install_outbound_targets
from .adapters import Detection, install_adapters, uninstall_adapters
from .packs import install_mcp_pack, present_provider_defaults, resolve_dev_cache

_TRUTHY = {"1", "true", "yes"}


def is_disabled(env: Mapping[str, str] | None = None) -> bool:
    env = env if env is not None else os.environ
    return env.get("KEEL_DISABLE", "").strip().lower() in _TRUTHY


class _State:
    installed: bool = False
    finder: KeelFinder | None = None
    discovery: Discovery | None = None
    exit_registered: bool = False
    mcp_uninstall: Any = None
    state: dict[str, Any] | None = None


_STATE = _State()


def install_keel(
    *, cwd: str | Path | None = None, env: Mapping[str, str] | None = None
) -> dict[str, Any]:
    """Install Keel's Tier 1 machinery. Idempotent within a process."""
    env = env if env is not None else os.environ
    if is_disabled(env):
        return {"enabled": False, "reason": "KEEL_DISABLE"}
    if _STATE.installed:
        # Return the SAME full state the first install produced (backend,
        # discovery, flow_entrypoints, …) rather than a bare marker — callers
        # like `_run.run_target` index into this dict unconditionally, and a
        # second `install_keel()` call in the same process (e.g. the .pth
        # shim's `keel._auto` installing before `_run.run_target` installs
        # again) must not silently drop that state (KEEL-… double-activation
        # regression). `_STATE.installed` is only ever set True in lockstep
        # with `_STATE.state` (right before this function returns below), so
        # reaching this branch guarantees `_STATE.state` is populated.
        return {**_STATE.state, "reason": "already-installed"}

    cwd = Path(cwd or Path.cwd())
    raw, source = load_policy(cwd)  # raises KEEL-E001 on unreadable/invalid TOML
    # Backend first: whether it's persistent (native + attached journal) decides
    # whether the LLM dev cache resolves to `scope=persistent` (cross-run replay).
    backend = load_backend(env.get("KEEL_BACKEND"), cwd=cwd, env=env)
    persistent = bool(getattr(backend, "persistent", False))
    # Layer the embedded pack defaults (and any present provider pack) UNDER the
    # user config, then resolve the LLM dev cache (`mode = "dev"` → a concrete
    # ttl off-prod, dropped when KEEL_ENV=prod; scope=persistent when the backend
    # can persist). Both steps mirror the Node front end exactly (parity).
    policy = resolve_dev_cache(
        apply_pack_defaults(raw, present_provider_defaults()), env, persistent=persistent
    )
    policy = apply_journal_env_override(policy, env)
    backend.configure(policy)  # raises KEEL-E001/KEEL-E005 on invalid/unsupported policy

    # The explicit `[target."…"]` keys of the SAME effective policy the core
    # just configured — discovery's "wrapped" classification (dx-spec §2's
    # coverage gap) must agree with what actually applied.
    known_targets = frozenset(policy.get("target") or {})
    discovery = Discovery(cwd, known_targets)
    _STATE.discovery = discovery
    set_runtime(backend, discovery)
    # Outbound host/URL-pattern matchers (docs/targeting.md), compiled from the
    # same effective policy the backend was configured with: the HTTP packs'
    # target judgment consults these so `[target."*.internal.corp"]`-style keys
    # actually select requests. The core still sees one exact key per call.
    install_outbound_targets(policy)

    targets = extract_function_targets(policy)
    _STATE.finder = install_import_hook(targets)

    # Tier 2 flow entrypoints (`[flows] entrypoints`, py:module:function) — the
    # runner consults these to decide whether `keel run <script>` is a durable
    # flow. Parsing only; running one requires the native backend.
    flow_entrypoints = extract_flow_entrypoints(policy)
    set_flow_entrypoints(flow_entrypoints)
    # `cmd:` flow entrypoints + their `[flows.match]` argv rules (CCR-5): the
    # subprocess adapter consults these to decide whether an intercepted
    # `subprocess.run`/`call` maps to a declared durable flow. Stored before
    # `install_adapters()` so the pack's `install()` sees them.
    set_cmd_flows(extract_cmd_flows(policy))

    # Library adapters (httpx/requests/…) plus framework packs with a real
    # seam of their own (adk_pack, pydantic-ai, …): all armed lazily — each
    # patches its library/framework only when the program imports it.
    # Present-but-unused libraries cost nothing, so `keel run` startup stays
    # cheap.
    adapters = install_adapters()

    # Framework packs: auto-detect and patch the MCP client SDK if present.
    # Best-effort — an absent SDK is a silent no-op; never fatal (mirrors the
    # Node front end's installMcpPack, called right after its fetch install).
    mcp = install_mcp_pack()
    _STATE.mcp_uninstall = mcp.get("uninstall") if mcp.get("active") else None

    _register_exit_flush()
    _banner(env, source, [t.key for t in targets], adapters, mcp)

    state = {
        "enabled": True,
        "backend": backend,
        "discovery": discovery,
        "source": source,
        "function_targets": targets,
        "flow_entrypoints": flow_entrypoints,
        "adapters": adapters,
        "mcp": mcp,
    }
    # Set together, at the very end, after every raise point above (load_policy's
    # KEEL-E001, backend.configure's KEEL-E001/KEEL-E005) has already passed:
    # a failed install must leave `_STATE.installed` False so a retry (e.g. the
    # NEXT `install_keel()` call in the same process) re-parses the policy and
    # surfaces the SAME loud error again, rather than wrongly believing itself
    # already-installed with no cached state to return (the AssertionError /
    # TypeError this ordering previously risked).
    _STATE.state = state
    _STATE.installed = True
    return state


def apply_journal_env_override(
    policy: dict[str, Any], env: Mapping[str, str]
) -> dict[str, Any]:
    """`KEEL_JOURNAL` is the journal escape hatch: when it is set in the
    environment (even to the empty string, which *disables* the journal), the
    construction-time selection made from it wins over keel.toml's `journal`
    key. The core honors the effective policy's `journal` at configure time, so
    the override is composed here — the key is dropped before `configure`,
    leaving the env-selected (or disabled) construction attachment in force.
    Precedence: KEEL_JOURNAL (when set) > policy `journal` > `.keel/journal.db`.
    Mirrors the Node front end exactly (parity)."""
    if "KEEL_JOURNAL" not in env or "journal" not in policy:
        return policy
    return {k: v for k, v in policy.items() if k != "journal"}


def uninstall_keel() -> None:
    """Restore the pre-install state (uninstall-clean / test teardown).

    Removes the import hook and clears the runtime so any already-installed
    wrappers become transparent passthroughs, and closes the discovery store.
    """
    remove_import_hook(_STATE.finder)
    _STATE.finder = None
    uninstall_adapters()
    if _STATE.mcp_uninstall is not None:
        _STATE.mcp_uninstall()
        _STATE.mcp_uninstall = None
    if _STATE.discovery is not None:
        _STATE.discovery.close()
        _STATE.discovery = None
    clear_outbound_targets()
    clear_runtime()
    _STATE.installed = False
    _STATE.state = None


def _register_exit_flush() -> None:
    if _STATE.exit_registered:
        return
    _STATE.exit_registered = True

    def _flush() -> None:
        if _STATE.discovery is not None:
            _STATE.discovery.close()
        # The native engine's live NDJSON event feed (`.keel/events/`,
        # `KEEL_EVENTS`) flushes its writer thread whenever the queue drains,
        # which a long-lived `keel tail`'d process never needs help with —
        # but a short-lived `keel run`/`keel sim` script can exit before its
        # last few events land on disk. Read the CURRENT runtime backend
        # (`_runtime.get_backend()`, not a snapshot taken at registration
        # time) since `keel run`/`keel sim` may have since wrapped it in a
        # RecordingBackend/SimBackend that delegates `flush_events` through.
        # Best-effort: the stub backend has no such method.
        flush_events = getattr(get_backend(), "flush_events", None)
        if callable(flush_events):
            flush_events()

    atexit.register(_flush)


def _banner(
    env: Mapping[str, str],
    source: str,
    target_keys: list[str],
    adapters: list[Detection],
    mcp: dict[str, Any] | None = None,
) -> None:
    if env.get("KEEL_QUIET", "").strip().lower() in _TRUTHY:
        return
    desc = "production defaults" if source == "defaults" else "policy keel.toml"
    # One line, dx-spec format (§ "wrapped N call sites (…) with … — keel init"),
    # listing function call sites and armed adapters together. At Level 0 there
    # are no function targets, so we show the adapters rather than "0 call sites".
    pieces: list[str] = []
    n = len(target_keys)
    if n:
        noun = "call site" if n == 1 else "call sites"
        pieces.append(f"{n} {noun} ({', '.join(sorted(target_keys))})")
    if adapters:
        pieces.append(", ".join(f"{d.name} {d.version}".strip() for d in adapters))
    if mcp and mcp.get("active"):
        pieces.append("mcp: transports")
    wrapped = " + ".join(pieces) if pieces else "nothing yet"
    sys.stderr.write(f"keel ▸ wrapped {wrapped} with {desc} — `keel init` to customize\n")
