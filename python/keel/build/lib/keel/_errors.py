"""The front end's own error type, carrying a frozen KEEL-E0NN code.

Only used for failures the *front end* originates before the backend is
involved — chiefly policy loading (KEEL-E001) and backend selection
(KEEL-E040). Backend validation errors (from `configure`) carry their own
code and are allowed to propagate unchanged; `is_keel_error` recognises both
by duck-typing on `.code`, so the runner can render either without importing
a specific backend's exception class.

Codes are the frozen taxonomy in contracts/error-codes.json; this module
never invents a code outside it.
"""

from __future__ import annotations


class KeelError(Exception):
    """A Keel configuration/bootstrap error tagged with a KEEL-E0NN code."""

    def __init__(self, code: str, message: str) -> None:
        super().__init__(f"{code}: {message}")
        self.code = code
        self.message = message


def is_keel_error(exc: BaseException) -> bool:
    """True if `exc` is a Keel error (ours or a backend's) — a `.code` that
    looks like the frozen taxonomy. Lets the runner treat a config error the
    same whether it came from us or from the backend's `configure`."""
    code = getattr(exc, "code", None)
    return isinstance(code, str) and code.startswith("KEEL-E")
