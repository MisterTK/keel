"""The generated wrapper for `py:` function targets.

`wrap_function` turns a plain module-level function into one that routes each
call through the backend's `execute`, then re-raises the ORIGINAL exception on
terminal failure (DX invariant 5). The original source is never edited; the
wrapper is installed onto the module by the import hook (`_hook`).

Contract (matches the Node loader-runtime): listing a `py:` target in
keel.toml is the user's explicit assertion that the function is safe to
retry, so wrapped calls are marked `idempotent=True`. A raised exception is
error class `other`, which is NOT in the default `retry.on` — so by default a
function failure propagates unchanged (no retry); add `other` to the target's
`retry.on` to retry function failures.
"""

from __future__ import annotations

import functools
import hashlib
import time
from typing import Any, Callable

from . import _runtime

ENVELOPE_VERSION = 1

#: Marker set on every wrapper so double-wrapping (hook + retroactive pass, or
#: a re-import) is a no-op, and tooling can recognise a Keel-wrapped function.
WRAPPED_ATTR = "__keel_wrapped__"


def is_wrapped(fn: Any) -> bool:
    return getattr(fn, WRAPPED_ATTR, False) is True


def _args_hash(args: tuple[Any, ...], kwargs: dict[str, Any]) -> str | None:
    """A stable SHA-256 over the repr-normalised call arguments, or None when
    they can't be represented (a custom `__repr__` that raises). Only used as
    a cache key, so None simply disables caching for that call — never an
    error."""
    try:
        norm = repr((args, tuple(sorted(kwargs.items()))))
    except Exception:
        return None
    return hashlib.sha256(norm.encode("utf-8", "surrogatepass")).hexdigest()


def _attach_outcome(exc: BaseException, outcome: dict[str, Any]) -> None:
    """Attach the core outcome for those who look, without ever letting the
    attachment interfere with re-raising the original exception."""
    try:
        exc.keel_outcome = outcome  # type: ignore[attr-defined]
    except Exception:
        pass


def wrap_function(target: str, op: str, fn: Callable[..., Any]) -> Callable[..., Any]:
    """Wrap `fn` for the policy `target` (the resolved key), reporting the
    concrete `op` id (`py:module.func`) in messages and discovery.

    `target` is the policy key so the backend's resolver applies the exact
    entry the user wrote (including a glob like `py:pkg.enrich.*`); `op` is the
    concrete function id, so a failure message names the real function.
    """

    @functools.wraps(fn)
    def wrapper(*args: Any, **kwargs: Any) -> Any:
        backend = _runtime.get_backend()
        if backend is None:
            return fn(*args, **kwargs)  # disabled / uninstalled: transparent

        request = {
            "v": ENVELOPE_VERSION,
            "target": target,
            "op": op,
            "idempotent": True,
            "args_hash": _args_hash(args, kwargs),
        }

        def effect(_attempt: int) -> dict[str, Any]:
            try:
                return {"status": "ok", "payload": fn(*args, **kwargs)}
            except Exception as err:  # not BaseException: let exit/interrupt fly
                return {
                    "status": "error",
                    "class": "other",
                    "message": str(err),
                    "original": err,
                }

        started = time.perf_counter()
        outcome = backend.execute(request, effect)
        latency_ms = round((time.perf_counter() - started) * 1000)

        discovery = _runtime.get_discovery()
        if discovery is not None:
            discovery.record(target, outcome, latency_ms)

        if outcome.get("result") == "ok":
            return outcome.get("payload")

        err = outcome.get("error") or {}
        original = err.get("original")
        if original is not None:
            _attach_outcome(original, outcome)
            raise original
        # No round-tripped original (e.g. breaker fast-fail): surface the
        # core's own error, still carrying the outcome.
        synthetic = RuntimeError(err.get("message") or "keel: call failed")
        synthetic.code = err.get("code")  # type: ignore[attr-defined]
        _attach_outcome(synthetic, outcome)
        raise synthetic

    setattr(wrapper, WRAPPED_ATTR, True)
    return wrapper
