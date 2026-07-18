"""Auto-activation shim: the import target of ``keelrun_activate.pth``.

The `.pth` line (shipped at site-packages root by the keelrun wheel) checks
``KEEL_ENABLE`` BEFORE importing anything from keel — an idle install costs
one ``os.environ.get`` at interpreter startup. When the gate is on, this
module runs the exact same in-process bootstrap as ``keel run``
(:func:`keel.bootstrap.install_keel`): policy from ``<root>/keel.toml``,
backend, import hook, adapters, MCP pack, exit flush, banner. ``KEEL_CWD``
(optional) relocates that root — needed when the process cwd is a project
root but keel.toml lives in the deployable app directory (the agents-cli
container layout). ``KEEL_DISABLE`` wins over ``KEEL_ENABLE``, same as
everywhere else.

What this deliberately does NOT do (those are ``keel run``'s CLI-side jobs,
not bootstrap): the preflight resilience advisory, sys.path/argv shaping,
``KEEL_SIM_PLAN``/``KEEL_RECORD`` wiring, and flow-entrypoint DISPATCH — a
`.pth` has no "target script" to match ``[flows] entrypoints`` against
(entrypoint functions still journal correctly when the wrapped app calls
them under an open flow; designating a process entrypoint stays a
``keel run`` feature).

Failure contract (spec §4): activation must never take down the host — any
exception becomes ONE stderr line and the app continues unwrapped.
"""

from __future__ import annotations

import os
import sys

#: Same truthy convention as ``bootstrap._TRUTHY``.
_TRUTHY = {"1", "true", "yes"}


def _activate() -> None:
    if os.environ.get("KEEL_ENABLE", "").strip().lower() not in _TRUTHY:
        return  # belt and suspenders: the .pth line already gated on this
    try:
        from .bootstrap import install_keel, is_disabled

        if is_disabled(os.environ):
            return
        install_keel(cwd=os.environ.get("KEEL_CWD") or None, env=os.environ)
    except Exception as err:  # noqa: BLE001 — the host app must survive us
        sys.stderr.write(f"keel ▸ auto-activation failed ({err}); continuing without keel\n")


_activate()
