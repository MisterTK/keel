"""Adapter lifecycle invariants: KEEL_DISABLE leaves libraries completely
unpatched, install/uninstall is reversible to byte-identity on the seam
attributes, detection is honest, and the pack dataclasses match the frozen
contract."""

from __future__ import annotations

import dataclasses
import importlib.util
import sys
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

import httpx
from requests.adapters import HTTPAdapter

from keel import _runtime, bootstrap
from keel.adapters import (
    _AdapterFinder,
    _pack,
    available_packs,
    httpx_pack,
    install_adapters,
    requests_pack,
    uninstall_adapters,
)

from . import CONTRACTS


def _seam_attrs() -> dict[str, object]:
    return {
        "httpx.sync": httpx.HTTPTransport.handle_request,
        "httpx.async": httpx.AsyncHTTPTransport.handle_async_request,
        "httpx.Client.__init__": httpx.Client.__init__,
        "httpx.AsyncClient.__init__": httpx.AsyncClient.__init__,
        "requests.send": HTTPAdapter.send,
    }


class DisableIdentityTest(unittest.TestCase):
    def test_keel_disable_leaves_seams_unpatched(self) -> None:
        pristine = _seam_attrs()
        self.addCleanup(bootstrap.uninstall_keel)
        with TemporaryDirectory() as d:
            result = bootstrap.install_keel(cwd=d, env={"KEEL_DISABLE": "1"})
        self.assertEqual(result, {"enabled": False, "reason": "KEEL_DISABLE"})
        for name, obj in _seam_attrs().items():
            self.assertIs(obj, pristine[name], f"{name} must be untouched under KEEL_DISABLE")
        self.assertFalse(
            any(isinstance(f, _AdapterFinder) for f in sys.meta_path),
            "no adapter finder is registered under KEEL_DISABLE",
        )


class ReversibilityTest(unittest.TestCase):
    def test_install_then_uninstall_restores_seam_identity(self) -> None:
        pristine = _seam_attrs()
        install_adapters()
        try:
            after_install = _seam_attrs()
            for name in pristine:
                self.assertIsNot(after_install[name], pristine[name], f"{name} patched on install")
            self.assertTrue(any(isinstance(f, _AdapterFinder) for f in sys.meta_path))
        finally:
            uninstall_adapters()
        restored = _seam_attrs()
        for name in pristine:
            self.assertIs(restored[name], pristine[name], f"{name} restored on uninstall")
        self.assertFalse(any(isinstance(f, _AdapterFinder) for f in sys.meta_path))

    def test_custom_transport_instance_wrap_is_removed_on_uninstall(self) -> None:
        _runtime.clear_runtime()  # a transparent runtime: no I/O when the client is built
        install_adapters()
        transport = httpx.MockTransport(lambda request: httpx.Response(200))
        with httpx.Client(transport=transport):
            pass
        self.assertIn("handle_request", vars(transport), "custom transport wrapped at client init")
        uninstall_adapters()
        self.assertNotIn("handle_request", vars(transport), "instance wrap removed on uninstall")


class DetectTest(unittest.TestCase):
    def test_httpx_detects_pinned(self) -> None:
        d = httpx_pack.detect()
        self.assertTrue(d.matched)
        self.assertEqual(d.name, "httpx")
        self.assertTrue(d.version)
        self.assertEqual(d.confidence, "pinned")

    def test_requests_detects_pinned(self) -> None:
        d = requests_pack.detect()
        self.assertTrue(d.matched)
        self.assertEqual(d.name, "requests")
        self.assertEqual(d.confidence, "pinned")

    def test_available_packs_reports_both(self) -> None:
        packs = available_packs()
        self.assertEqual({d.name for d in packs}, {"httpx", "requests"})
        self.assertTrue(all(d.matched for d in packs))


class ContractParityTest(unittest.TestCase):
    """The runtime pack dataclasses must match the frozen contract shape
    (contracts/stubs/adapter_pack.py) field-for-field."""

    def _contract_module(self):  # type: ignore[no-untyped-def]
        path = Path(CONTRACTS) / "stubs" / "adapter_pack.py"
        spec = importlib.util.spec_from_file_location("_contract_adapter_pack", path)
        assert spec and spec.loader
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        return module

    def test_dataclasses_match_contract(self) -> None:
        contract = self._contract_module()
        for name in ("Detection", "Seam", "TargetDecl"):
            ours = getattr(_pack, name)
            theirs = getattr(contract, name)
            self.assertEqual(
                [(f.name, f.default) for f in dataclasses.fields(ours)],
                [(f.name, f.default) for f in dataclasses.fields(theirs)],
                f"{name} fields/defaults must match the frozen contract",
            )


if __name__ == "__main__":
    unittest.main()
