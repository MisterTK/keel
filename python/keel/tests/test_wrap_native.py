"""Function-target delivery across the live-object boundary — on BOTH backends.

The load-bearing invariant (parity with `node/keel/src/loader-runtime.mjs`): a
`py:` function target returns the LIVE object on success and re-raises the LIVE
exception on terminal failure — even under the native `keel_core` backend, which
cannot round-trip a live Python object/exception through the FFI. These tests
drive `wrap_function` through `load_backend("stub")` AND `load_backend("native")`
and assert identical semantics; the native leg skips when `keel_core` is absent.

Regression guard for the whole-branch finding "py: function targets send live
Python objects through the native core; success becomes RuntimeError and original
exceptions are never re-raised". The stub already had exception-identity coverage
(`test_wrap.py`); this adds the native seam plus non-JSON returns and tuple
identity on both backends.
"""

from __future__ import annotations

import gc
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory
from typing import Any

from keel import _runtime
from keel._backend import load_backend
from keel._wrap import wrap_function

try:
    import keel_core  # noqa: F401

    _NATIVE = True
except ImportError:
    _NATIVE = False

_RETRY_OTHER = {"attempts": 3, "on": ["other"], "schedule": "fixed(1ms)"}


class CustomError(Exception):
    """A user exception type Keel must re-raise by identity (DX invariant 5)."""


class Dataclassy:
    """A deliberately NON-JSON-serializable return value (json.dumps raises)."""

    def __init__(self, n: int) -> None:
        self.n = n


class _FunctionTargetContract:
    """Shared contract exercised against each backend (subclasses set KIND)."""

    KIND = "stub"

    def _install(self, policy: dict[str, Any]) -> Any:
        backend = load_backend(self.KIND, cwd=self.cwd)
        backend.configure(policy)
        _runtime.set_runtime(backend, None)  # discovery not needed here
        return backend

    def setUp(self) -> None:
        self._tmp = TemporaryDirectory()
        self.cwd = Path(self._tmp.name)

    def tearDown(self) -> None:
        _runtime.clear_runtime()
        gc.collect()  # let the native core drop its journal connection now
        self._tmp.cleanup()

    def test_terminal_failure_reraises_live_exception_identity(self) -> None:
        # Retryable (class other in retry.on), attempts exhausted → KEEL-E010,
        # and the caller must catch the EXACT original object, not a RuntimeError.
        self._install({"target": {"py:m.boom": {"retry": {**_RETRY_OTHER, "attempts": 2}}}})
        original = CustomError("provider blew up")

        def boom() -> None:
            raise original

        wrapped = wrap_function("py:m.boom", "py:m.boom", boom)
        with self.assertRaises(CustomError) as ctx:
            wrapped()
        self.assertIs(ctx.exception, original, f"[{self.KIND}] must re-raise the live exception")
        self.assertEqual(ctx.exception.keel_outcome["error"]["code"], "KEEL-E010")

    def test_non_retryable_reraises_live_exception(self) -> None:
        # retry.on lists only "timeout"; a raised exception is class "other" →
        # non-retryable → KEEL-E015, live exception re-raised on attempt 1.
        self._install(
            {"target": {"py:m.bad": {"retry": {"attempts": 5, "on": ["timeout"], "schedule": "fixed(1ms)"}}}}
        )
        original = CustomError("nope")

        def bad() -> None:
            raise original

        wrapped = wrap_function("py:m.bad", "py:m.bad", bad)
        with self.assertRaises(CustomError) as ctx:
            wrapped()
        self.assertIs(ctx.exception, original, f"[{self.KIND}] must re-raise the live exception")
        self.assertEqual(ctx.exception.keel_outcome["error"]["code"], "KEEL-E015")

    def test_non_json_return_delivered_live(self) -> None:
        # A SUCCESSFUL call returning a non-JSON object must return that live
        # object — never raise (the bug turned success into RuntimeError E015).
        self._install({"target": {"py:m.obj": {}}})
        sentinel = Dataclassy(7)
        wrapped = wrap_function("py:m.obj", "py:m.obj", lambda: sentinel)
        got = wrapped()
        self.assertIs(got, sentinel, f"[{self.KIND}] live non-JSON return delivered by identity")

    def test_tuple_return_identity_preserved(self) -> None:
        # A live tuple stays a tuple (must not be depythonized to a list).
        self._install({"target": {"py:m.tup": {}}})
        tup = (1, "two", 3.0)
        wrapped = wrap_function("py:m.tup", "py:m.tup", lambda: tup)
        got = wrapped()
        self.assertIs(got, tup, f"[{self.KIND}] live tuple returned unchanged")
        self.assertIsInstance(got, tuple)


class FunctionTargetContractStub(_FunctionTargetContract, unittest.TestCase):
    KIND = "stub"


@unittest.skipUnless(_NATIVE, "keel_core native module not built (maturin develop in crates/keel-py)")
class FunctionTargetContractNative(_FunctionTargetContract, unittest.TestCase):
    KIND = "native"

    def test_native_is_actually_selected(self) -> None:
        backend = self._install({"target": {"py:m.x": {}}})
        self.assertTrue(hasattr(backend, "execute_async"), "native exposes execute_async")
        self.assertNotEqual(type(backend).__module__, "keel_core_stub")


if __name__ == "__main__":
    unittest.main()
