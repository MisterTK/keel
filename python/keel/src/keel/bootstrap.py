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
import time
from pathlib import Path
from typing import Any, Mapping

from . import __version__
from ._backend import _stub_paused, backend_name, load_backend
from ._cachepoll import CachePollDetector
from ._defaults import apply_pack_defaults
from ._deploy import ephemeral_journal_warning
from ._discovery import Discovery
from ._hook import KeelFinder, install_import_hook, remove_import_hook
from ._log import emit, json_logs
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
from ._summary import Summary, format_summary, format_summary_json, keel_on_path
from .adapters import Detection, install_adapters, uninstall_adapters
from .packs import (
    dev_cache_off_reason,
    install_mcp_pack,
    present_provider_defaults,
    resolve_dev_cache,
    serverless_marker,
)

_TRUTHY = {"1", "true", "yes"}


def is_disabled(env: Mapping[str, str] | None = None) -> bool:
    env = env if env is not None else os.environ
    return env.get("KEEL_DISABLE", "").strip().lower() in _TRUTHY


def policy_optional(env: Mapping[str, str] | None = None) -> bool:
    """`KEEL_POLICY=optional` restores the pre-0.5.5 fallback: an explicit
    KEEL_CWD with no keel.toml runs on production defaults (with a warning)
    instead of refusing to activate. The only recognized value."""
    env = env if env is not None else os.environ
    return env.get("KEEL_POLICY", "").strip().lower() == "optional"


def missing_policy_error(root: Path) -> str:
    return (
        f"keel ▸ error: KEEL_CWD={root} is set but {root / 'keel.toml'} does not exist — "
        "Keel NOT activated; the app continues without keel "
        "(set KEEL_POLICY=optional to run on production defaults instead)\n"
    )


def _console_enabled(policy: Mapping[str, Any], env: Mapping[str, str]) -> bool:
    if env.get("KEEL_QUIET", "").strip().lower() in _TRUTHY:
        return False
    telemetry = policy.get("telemetry")
    if not isinstance(telemetry, dict):
        return True  # schema default: console = true
    return bool(telemetry.get("console", True))


class _State:
    installed: bool = False
    finder: KeelFinder | None = None
    discovery: Discovery | None = None
    summary: Summary | None = None
    exit_registered: bool = False
    mcp_uninstall: Any = None
    state: dict[str, Any] | None = None
    refused: dict[str, Any] | None = None
    # The `(cwd, cwd_source)` the refusal above was printed FOR. The latch is a
    # print-once latch, not a process-wide verdict: a later call naming a
    # DIFFERENT root was never asked about (see `install_keel`).
    refused_key: tuple[str, str] | None = None
    # Captured for the atexit flush, which runs long after `install_keel`
    # returned and has no other way to reach the environment (KEEL_LOG_FORMAT)
    # or the policy provenance the JSON summary reports.
    env: Mapping[str, str] | None = None
    meta: dict[str, Any] | None = None


_STATE = _State()


def install_keel(
    *,
    cwd: str | Path | None = None,
    env: Mapping[str, str] | None = None,
    cwd_source: str = "cwd",
) -> dict[str, Any]:
    """Install Keel's Tier 1 machinery. Idempotent within a process.

    `cwd_source` is "KEEL_CWD" when `cwd` came from that env var: pointing
    KEEL_CWD at a directory with no keel.toml is a misconfiguration (the
    policy did not ship — issue #85's second occurrence), so Keel refuses to
    activate rather than silently running production defaults.
    """
    env = env if env is not None else os.environ
    if is_disabled(env):
        return {"enabled": False, "reason": "KEEL_DISABLE"}
    cwd = Path(cwd or Path.cwd())
    # The refusal is printed once per process, but the latch is keyed on WHAT
    # was refused: a later `install_keel(cwd=…)` naming a DIFFERENT root (the
    # public API, and what `_run.run_target` uses after the `.pth` shim already
    # ran) was never asked about, and must be answered on its own merits rather
    # than inheriting a stale refusal. Node twin: `bootstrap.mjs`.
    if _STATE.refused is not None and _STATE.refused_key == (str(cwd), cwd_source):
        return dict(_STATE.refused)  # already said so once this process
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

    raw, source = load_policy(cwd)  # raises KEEL-E001 on unreadable/invalid TOML
    if source == "defaults" and cwd_source == "KEEL_CWD" and not policy_optional(env):
        text = missing_policy_error(cwd)
        emit(
            env,
            text,
            {
                "keel": "error",
                "code": "policy-missing-at-keel-cwd",
                "keel_cwd": str(cwd),
                "message": text[len("keel ▸ error: ") :].rstrip("\n"),
                "version": __version__,
            },
        )
        _STATE.refused = {"enabled": False, "reason": "policy-missing-at-keel-cwd", "root": str(cwd)}
        _STATE.refused_key = (str(cwd), cwd_source)
        return dict(_STATE.refused)
    # Policy provenance, captured once: the two fields ("did my policy ship?",
    # "how many calls were served from cache?") that would have named the
    # 2026-09-15 outage in one log query (F10). Read again at exit by `_flush`.
    _STATE.env = env
    _STATE.meta = {
        "keel_cwd": env.get("KEEL_CWD") or None,
        "policy_path": str(cwd / "keel.toml") if source != "defaults" else None,
        "policy_source": source,
        "version": __version__,
    }
    # Backend first: whether it's persistent (native + attached journal) decides
    # whether the LLM dev cache resolves to `scope=persistent` (cross-run replay).
    backend = load_backend(env.get("KEEL_BACKEND"), cwd=cwd, env=env)
    persistent = bool(getattr(backend, "persistent", False))
    # #119 transparency: which backend actually resolved (the DEFAULT
    # `KEEL_BACKEND=auto` falls back to the stub silently on a failed native
    # import) — carried in `_STATE.meta` so both the banner and the JSON
    # summary can say so, and folded into the activation row below for free.
    bname = backend_name(backend)
    _STATE.meta["backend"] = bname
    # Layer the embedded pack defaults (and any present provider pack) UNDER the
    # user config, then resolve the LLM dev cache (`mode = "dev"` → a concrete
    # ttl off-prod, dropped when KEEL_ENV=prod; scope=persistent when the backend
    # can persist). Both steps mirror the Node front end exactly (parity).
    policy = resolve_dev_cache(
        apply_pack_defaults(raw, present_provider_defaults()), env, persistent=persistent
    )
    policy = apply_journal_env_override(policy, env)
    backend.configure(policy)  # raises KEEL-E001/KEEL-E005 on invalid/unsupported policy
    # Issue #90: durable flows on a SQLite journal that will not survive an
    # instance replacement — doctor can see this from a deploy artifact in the
    # repo, but only the runtime can see the environment (serverless markers,
    # /.dockerenv, a read-only cwd).
    warning = ephemeral_journal_warning(policy, env, cwd)
    if warning is not None:
        emit(env, *warning)

    # The explicit `[target."…"]` keys of the SAME effective policy the core
    # just configured — discovery's "wrapped" classification (dx-spec §2's
    # coverage gap) must agree with what actually applied.
    known_targets = frozenset(policy.get("target") or {})
    # `[telemetry].console` (schema default true) gates the exit-time summary;
    # the raw dict is what we hold, so re-derive the default here (as
    # `_policy.extract_cmd_flows` does for `on_busy`). KEEL_QUIET silences it
    # exactly as it silences the banner.
    summary = Summary() if _console_enabled(policy, env) else None

    def _cache_poll_suspect(target: str, hits: int, span_s: int) -> None:
        emit(
            env,
            f"keel ▸ warning: {target} served {hits} consecutive cache hits for one identical "
            f"call over {span_s}s — if this is a status poll, set cache = "
            "{ mode = \"off\" } on that target "
            "— or give the status route its own poll policy (README: Poll)\n",
            {
                "keel": "warning",
                "code": "cache-poll-suspect",
                "target": target,
                "hits": hits,
                "span_s": span_s,
                "version": __version__,
            },
        )

    cachepoll = CachePollDetector(on_suspect=_cache_poll_suspect)
    discovery = Discovery(cwd, known_targets, summary=summary, cachepoll=cachepoll)
    _STATE.discovery = discovery
    _STATE.summary = summary
    set_runtime(backend, discovery)

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
    cmd_flows = extract_cmd_flows(policy)
    set_cmd_flows(cmd_flows)

    # #104: register the exit-flush hook BEFORE the row below is remembered.
    # `record_activation` only queues the row in memory (written lazily on
    # the first outcome or at close) — the atexit hook below is what
    # guarantees a well-behaved process flushes it at all, so it must exist
    # before a later best-effort failure (e.g. install_adapters raising)
    # could otherwise leave the row queued forever with no hook to flush it.
    # `_register_exit_flush` only reads `_STATE` lazily inside `_flush`, so
    # it has no dependency on anything constructed after this point.
    _register_exit_flush()

    # One activation row per process (WS8, #92): the evidence answering "was
    # Keel on, with which policy?" after a deployment. Spread `_STATE.meta`
    # (the same provenance the banner/JSON summary report) so this can never
    # drift from what Keel tells the user; the fields below are the ones
    # `_STATE.meta` doesn't carry.
    discovery.record_activation({
        **_STATE.meta,
        "ts_ms": int(time.time() * 1000),
        "pid": os.getpid(),
        "language": "python",
        "cwd": str(cwd),
        "flows_configured": bool(flow_entrypoints) or bool(cmd_flows),
        "argv0": sys.argv[0] if sys.argv else "",
    })

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

    _banner(env, source, [t.key for t in targets], adapters, mcp, cwd, cwd_source, backend_name=bname)

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
    _STATE.summary = None
    clear_runtime()
    _STATE.installed = False
    _STATE.state = None
    _STATE.refused = None
    _STATE.refused_key = None
    _STATE.env = None
    _STATE.meta = None


def _register_exit_flush() -> None:
    if _STATE.exit_registered:
        return
    _STATE.exit_registered = True

    def _flush() -> None:
        # The console summary goes first (design spec Part A: "prints before
        # the store closes"). It reads only its own counters and never raises.
        if _STATE.summary is not None:
            try:
                counts = _STATE.summary.counts()
                by_target = _STATE.summary.unprotected_by_target()
                if json_logs(_STATE.env if _STATE.env is not None else os.environ):
                    # Unconditional, unlike the text form: a zero line proves
                    # Keel was live and intercepted nothing, which is exactly
                    # what the outage post-mortem had no way to establish.
                    sys.stderr.write(
                        format_summary_json(
                            counts,
                            _STATE.meta or {},
                            by_target,
                            _STATE.summary.cache_poll_suspects(),
                        )
                    )
                else:
                    text = format_summary(counts, keel_on_path(), by_target)
                    if text:
                        sys.stderr.write(text)
            except Exception:  # noqa: BLE001 — observability never fails the process
                pass
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


def _policy_above_cwd(cwd: str | Path, max_levels: int = 8) -> Path | None:
    """#85: a server launched with its cwd in a subdirectory (e.g. uvicorn
    with `cwd=agents/`) makes `load_policy(cwd)` find no `keel.toml` and fall
    back to Level 0 defaults — silently, since that fallback is normal and
    intentional for a genuinely unconfigured project. This walk exists only
    to tell the two cases apart for the banner: is there a `keel.toml` one of
    the NEXT (at most `max_levels`) parent directories up from `cwd` never
    looked at?

    Fail-open by construction: any `OSError` while walking (permissions,
    a vanished directory, …) returns None rather than raising — this is
    purely cosmetic (the banner), never load-bearing for policy resolution,
    which already ran and already decided "defaults" before this is called.
    Mirrors the Node front end's walk (`node/keel/src/bootstrap.mjs`) and the
    agents-cli/doctor bounded-parent-walk convention (8 levels, stop at the
    filesystem root).
    """
    try:
        current = Path(cwd)
        for _ in range(max_levels):
            parent = current.parent
            if parent == current:  # reached the filesystem root
                return None
            if (parent / "keel.toml").exists():
                return parent
            current = parent
    except OSError:
        return None
    return None


def _banner(
    env: Mapping[str, str],
    source: str,
    target_keys: list[str],
    adapters: list[Detection],
    mcp: dict[str, Any] | None = None,
    cwd: str | Path | None = None,
    cwd_source: str = "cwd",
    *,
    backend_name: str = "native",
) -> None:
    if env.get("KEEL_QUIET", "").strip().lower() in _TRUTHY:
        return
    root = Path(cwd) if cwd is not None else None
    if source == "defaults":
        desc = "production defaults"
    elif root is not None:
        desc = f"policy {root / 'keel.toml'}"
    else:
        desc = "policy keel.toml"
    # WHY the dev cache is off, read from the SAME resolution the cache itself
    # uses (`dev_cache_off_reason`), so the JSON form can carry it as a field:
    # "was the dev cache on in that container, and if not why" is one of the
    # questions the 2026-09-15 post-mortem had to answer by inference, and a
    # log pipeline can only index what the object names (F10). Note this is
    # WIDER than the banner's parenthetical, which stays marker-only prose
    # (byte-unchanged): an explicit `KEEL_ENV=prod` also turns the cache off,
    # and a field named `dev_cache_off` reporting null there would be a lie.
    dev_cache_off = dev_cache_off_reason(env)
    if not env.get("KEEL_ENV", "").strip():
        marker = serverless_marker(env)
        if marker is not None:
            desc = f"{desc} (dev cache off: {marker} detected)"
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
    head = f"keel ▸ wrapped {wrapped} with {desc}"
    # `note` is the text after the em-dash — the one place the four tail
    # variants differ. Held as a value (rather than four `write` calls) so the
    # JSON form can carry it as a field; the text assembled below is
    # byte-identical to what each variant used to write directly.
    note: str | None
    if source != "defaults":
        note = None
    else:
        # #85: a keel.toml above cwd that load_policy never looked at.
        found = _policy_above_cwd(root) if root is not None else None
        if found is not None:
            note = (
                f"found keel.toml at {found} but running from {root}; "
                f"set KEEL_CWD={found} to load it"
            )
        elif cwd_source == "KEEL_CWD" and root is not None:
            # Only reachable under KEEL_POLICY=optional (install_keel refuses otherwise).
            note = (
                f"KEEL_CWD={root} is set but {root / 'keel.toml'} does not exist "
                "(KEEL_POLICY=optional)"
            )
        elif root is not None:
            note = f"no keel.toml in {root}; `keel init` to customize"
        else:
            note = "`keel init` to customize"
    # #119: the pure-Python stub differs from the native core in Tier 2 support
    # and cache persistence — say so, but ONLY off the common path. The native
    # line must stay byte-identical (golden tests pin it). This is its own
    # TRAILING segment, not part of `desc`: inside `desc` it would land between
    # "production defaults" and the em-dash (breaking the adjacency the
    # KEEL_CWD/KEEL_POLICY tests assert) and stack a second parenthetical onto
    # the serverless "(dev cache off: …)" one.
    if backend_name == "native":
        backend_note = ""
    elif _stub_paused(env):
        # #121: KEEL_STUB_PAUSED reinstates the entire #119 defect (every
        # duration silently reinterpreted, no backoff/throttling/pacing) —
        # an unstable, test-only seam that must never be silent if it ever
        # escapes into a real process. Appended to the SAME clause rather
        # than a new one, so the native line stays untouched either way.
        backend_note = (
            " (pure-Python backend: no durable flows, no cross-run cache; "
            "KEEL_STUB_PAUSED is set — no pacing: retry backoff, rate limiting "
            "and poll intervals are not real)"
        )
    else:
        backend_note = " (pure-Python backend: no durable flows, no cross-run cache)"
    body = head if note is None else f"{head} — {note}"
    text = f"{body}{backend_note}\n"
    obj: dict[str, Any] = {
        "dev_cache_off": dev_cache_off,
        "keel": "activation",
        "policy_path": str(root / "keel.toml") if source != "defaults" and root is not None else None,
        "policy_source": source,
        "root": str(root) if root is not None else None,
        "root_source": cwd_source,
        "version": __version__,
        "wrapped": wrapped,
    }
    if note is not None:
        obj["note"] = note
    emit(env, text, obj)
