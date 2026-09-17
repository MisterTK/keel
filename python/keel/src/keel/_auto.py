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
exception becomes ONE stderr line and the app continues unwrapped. That line
is a REFUSAL, not a warning (Keel is off, the host is serving unprotected),
so under ``KEEL_LOG_FORMAT=json`` it is the structured `activation-failed`
object at `severity: "ERROR"` — see :func:`_refusal`.
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
        keel_cwd = os.environ.get("KEEL_CWD") or None
        install_keel(
            cwd=keel_cwd,
            env=os.environ,
            cwd_source="KEEL_CWD" if keel_cwd else "cwd",
        )
    except Exception as err:  # noqa: BLE001 — the host app must survive us
        _refusal(err)


def _refusal(err: BaseException) -> None:
    """Report an activation failure: Keel is OFF and the host keeps serving.

    This is the SECOND of Keel's two activation refusals, and #130's defect
    applies to it in full: under ``KEEL_LOG_FORMAT=json`` an unstructured,
    unranked prose line is indistinguishable from a healthy start in any
    ``severity>=ERROR`` view. It reaches CCR-11's most likely rollout
    accident — a `keel.toml` carrying `until.absent` read by a Keel too old
    to know the key is KEEL-E001 here, which lands the whole process
    unprotected — so it carries the same `severity: "ERROR"` the
    `policy-missing-at-keel-cwd` refusal does.

    The text form is byte-identical to what it has always been, and
    ``node/keel/register.mjs`` is the twin: same code, same keys, same
    severity. Emitting is itself wrapped, because the exception being
    reported may be the very import that would have supplied ``emit`` — the
    fail-open contract (spec §4) outranks the structured form.
    """
    text = f"keel ▸ auto-activation failed ({err}); continuing without keel\n"
    try:
        from . import __version__
        from ._log import emit

        emit(
            os.environ,
            text,
            {
                "keel": "error",
                "code": "activation-failed",
                "keel_cwd": os.environ.get("KEEL_CWD") or None,
                "message": text[len("keel ▸ ") :].rstrip("\n"),
                "severity": "ERROR",
                "version": __version__,
            },
        )
    except Exception:  # noqa: BLE001 — never let reporting a failure be one
        sys.stderr.write(text)


_activate()
