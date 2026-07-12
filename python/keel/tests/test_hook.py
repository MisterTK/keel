"""The sys.meta_path import hook: wrap functions matching `py:` targets at
import time, preserve their metadata, honor glob selectivity, wrap
already-imported modules retroactively, and stay transparent when disabled."""

from __future__ import annotations

import importlib
import sys
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

from keel import _runtime
from keel._backend import load_backend
from keel._discovery import Discovery
from keel._hook import KeelFinder, install_import_hook, remove_import_hook
from keel._policy import FunctionTarget
from keel._wrap import is_wrapped

_MODULE = "sample_targets"


class HookTestBase(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = TemporaryDirectory()
        self._finder: KeelFinder | None = None
        sys.modules.pop(_MODULE, None)
        backend = load_backend("stub")
        backend.configure({"target": {}})
        self.backend = backend
        _runtime.set_runtime(backend, Discovery(Path(self._tmp.name)))

    def tearDown(self) -> None:
        remove_import_hook(self._finder)
        sys.modules.pop(_MODULE, None)
        _runtime.clear_runtime()
        self._tmp.cleanup()

    def arm(self, key: str, func_glob: str) -> None:
        self._finder = install_import_hook([FunctionTarget(key, _MODULE, func_glob)])


class ImportTimeWrappingTest(HookTestBase):
    def test_glob_selectivity_and_routing(self) -> None:
        self.arm("py:sample_targets.enrich_*", "enrich_*")
        mod = importlib.import_module(_MODULE)

        self.assertTrue(is_wrapped(mod.enrich_a))
        self.assertTrue(is_wrapped(mod.enrich_b))
        self.assertFalse(is_wrapped(mod.other), "glob must not select `other`")

        self.assertEqual(mod.enrich_a(1), 2)  # behavior unchanged
        # Routed through the backend under the matched policy key.
        self.assertIn("py:sample_targets.enrich_*", self.backend.report()["targets"])

    def test_metadata_preserved(self) -> None:
        self.arm("py:sample_targets.enrich_a", "enrich_a")
        mod = importlib.import_module(_MODULE)
        self.assertEqual(mod.enrich_a.__name__, "enrich_a")
        self.assertEqual(mod.enrich_a.__doc__, "doc-a")
        self.assertEqual(mod.enrich_a.__wrapped__.__name__, "enrich_a")
        self.assertFalse(is_wrapped(mod.enrich_a.__wrapped__), "original is unwrapped")

    def test_exact_target_wraps_only_that_function(self) -> None:
        self.arm("py:sample_targets.other", "other")
        mod = importlib.import_module(_MODULE)
        self.assertTrue(is_wrapped(mod.other))
        self.assertFalse(is_wrapped(mod.enrich_a))

    def test_transparent_when_backend_absent(self) -> None:
        # Hook installed and function wrapped, but runtime backend cleared:
        # the wrapper must fall straight through to the original.
        self.arm("py:sample_targets.enrich_a", "enrich_a")
        mod = importlib.import_module(_MODULE)
        _runtime.clear_runtime()
        self.assertEqual(mod.enrich_a(41), 42)


class RetroactiveWrappingTest(HookTestBase):
    def test_module_imported_before_install_is_wrapped(self) -> None:
        mod = importlib.import_module(_MODULE)  # imported BEFORE the hook
        self.assertFalse(is_wrapped(mod.enrich_a))
        self.arm("py:sample_targets.enrich_*", "enrich_*")  # retroactive pass
        self.assertTrue(is_wrapped(mod.enrich_a))
        self.assertTrue(is_wrapped(mod.enrich_b))
        self.assertFalse(is_wrapped(mod.other))


if __name__ == "__main__":
    unittest.main()
