"""The `keel run` runner: bootstrap, then execute the target script with
correct `__main__` semantics, argv, and exit-code passthrough.

Two entry shapes share one core (`run_target`):
  * `python -m keel run app.py [args...]`  → `main_module` (parses the `run`
    subcommand)
  * `keel-py-run app.py [args...]`         → `main_run_entry` (the internal
    console_script the public `keel run` CLI dispatches to)

When KEEL_DISABLE is set the script still runs, but with NO wrapping, NO
discovery, and NO policy load — byte-identical to `python app.py`.
"""

from __future__ import annotations

import os
import sys
from typing import Mapping, Sequence

from ._errors import is_keel_error

_USAGE_MODULE = "usage: python -m keel run <app.py> [args...]\n"
_USAGE_ENTRY = "usage: keel-py-run <app.py> [args...]\n"


def run_target(
    target: str,
    args: Sequence[str],
    *,
    cwd: str | None = None,
    env: Mapping[str, str] | None = None,
) -> None:
    """Bootstrap Keel (unless disabled), then run `target` as `__main__`.

    Never returns a value; a script's `sys.exit(n)` propagates as SystemExit
    so the process exit code passes through unchanged. A raised exception from
    the script also propagates unchanged (DX invariant 5).
    """
    import runpy

    env = env if env is not None else os.environ

    from .bootstrap import install_keel, is_disabled

    if not is_disabled(env):
        try:
            install_keel(cwd=cwd, env=env)
        except BaseException as exc:  # config error: loud, then exit 1
            if is_keel_error(exc):
                code = getattr(exc, "code", "KEEL-E040")
                message = getattr(exc, "message", str(exc))
                sys.stderr.write(f"keel ▸ {code}: {message}\n")
                raise SystemExit(1) from exc
            raise

    # Mirror CPython's `python <target>` semantics exactly. runpy.run_path
    # does NOT put the script's directory on sys.path for a file target, but a
    # direct interpreter launch does — so without this, sibling imports
    # (`import helpers` next to app.py) that work under plain python would break
    # under `keel run`, and byte-identity would fail for any script with a
    # directory component. Prepend dirname(abspath(target)), like CPython.
    sys.path.insert(0, os.path.dirname(os.path.abspath(target)))
    # Present argv exactly as `python <target> [args...]` would, so the script
    # sees the same argv[0] and byte-identical behavior.
    sys.argv = [target, *args]
    runpy.run_path(target, run_name="__main__")


def main_module(argv: Sequence[str] | None = None) -> None:
    """Entry for `python -m keel`: expects the `run` subcommand."""
    argv = list(sys.argv[1:] if argv is None else argv)
    if len(argv) >= 2 and argv[0] == "run":
        run_target(argv[1], argv[2:])
        return
    sys.stderr.write(_USAGE_MODULE)
    raise SystemExit(2)


def main_run_entry(argv: Sequence[str] | None = None) -> None:
    """Entry for the `keel-py-run` console_script: runs a script directly."""
    argv = list(sys.argv[1:] if argv is None else argv)
    if argv:
        run_target(argv[0], argv[1:])
        return
    sys.stderr.write(_USAGE_ENTRY)
    raise SystemExit(2)
