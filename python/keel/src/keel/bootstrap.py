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
from ._policy import extract_flow_entrypoints, extract_function_targets, load_policy
from ._runtime import clear_runtime, set_runtime
from .adapters import Detection, install_adapters, uninstall_adapters
from .packs import present_provider_defaults, resolve_dev_cache

_TRUTHY = {"1", "true", "yes"}


def is_disabled(env: Mapping[str, str] | None = None) -> bool:
    env = env if env is not None else os.environ
    return env.get("KEEL_DISABLE", "").strip().lower() in _TRUTHY


class _State:
    installed: bool = False
    finder: KeelFinder | None = None
    discovery: Discovery | None = None
    exit_registered: bool = False


_STATE = _State()


def install_keel(
    *, cwd: str | Path | None = None, env: Mapping[str, str] | None = None
) -> dict[str, Any]:
    """Install Keel's Tier 1 machinery. Idempotent within a process."""
    env = env if env is not None else os.environ
    if is_disabled(env):
        return {"enabled": False, "reason": "KEEL_DISABLE"}
    if _STATE.installed:
        return {"enabled": True, "reason": "already-installed"}
    _STATE.installed = True

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
    backend.configure(policy)  # raises KEEL-E001 on invalid policy (field paths)

    discovery = Discovery(cwd)
    _STATE.discovery = discovery
    set_runtime(backend, discovery)

    targets = extract_function_targets(policy)
    _STATE.finder = install_import_hook(targets)

    # Tier 2 flow entrypoints (`[flows] entrypoints`, py:module:function) — the
    # runner consults these to decide whether `keel run <script>` is a durable
    # flow. Parsing only; running one requires the native backend.
    flow_entrypoints = extract_flow_entrypoints(policy)

    # Library adapters (httpx/requests/…): armed lazily — each patches its
    # library only when the program imports it. Present-but-unused libraries
    # cost nothing, so `keel run` startup stays cheap.
    adapters = install_adapters()

    _register_exit_flush()
    _banner(env, source, [t.key for t in targets], adapters)

    return {
        "enabled": True,
        "backend": backend,
        "discovery": discovery,
        "source": source,
        "function_targets": targets,
        "flow_entrypoints": flow_entrypoints,
        "adapters": adapters,
    }


def uninstall_keel() -> None:
    """Restore the pre-install state (uninstall-clean / test teardown).

    Removes the import hook and clears the runtime so any already-installed
    wrappers become transparent passthroughs, and closes the discovery store.
    """
    remove_import_hook(_STATE.finder)
    _STATE.finder = None
    uninstall_adapters()
    if _STATE.discovery is not None:
        _STATE.discovery.close()
        _STATE.discovery = None
    clear_runtime()
    _STATE.installed = False


def _register_exit_flush() -> None:
    if _STATE.exit_registered:
        return
    _STATE.exit_registered = True

    def _flush() -> None:
        if _STATE.discovery is not None:
            _STATE.discovery.close()

    atexit.register(_flush)


def _banner(
    env: Mapping[str, str],
    source: str,
    target_keys: list[str],
    adapters: list[Detection],
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
    wrapped = " + ".join(pieces) if pieces else "nothing yet"
    sys.stderr.write(f"keel ▸ wrapped {wrapped} with {desc} — `keel init` to customize\n")
