"""Farm contract test: keel.adapters.urllib_pack against the REAL stdlib.

Runs ONLY under KEEL_ADAPTER_FARM=1 (see test_farm_adk.py's module docs for
the full convention). Stdlib adaptation of the three-layer convention: there
is no structural fake and no pip install — ``urllib.request`` is always
present, so the offline fast path (tests/test_adapters_urllib.py) already
exercises the real seam. This module is still the explicit certification
ritual, certifying on the running interpreter (the pack's REGISTRY row is
keyed to the Python runtime version — the documented stdlib convention
exception):

* ``urllib.request.OpenerDirector.open`` exists and its UNBOUND signature
  binds ``(self, fullurl, data, timeout)`` — the exact signature
  ``_open_wrapper`` re-declares;
* the running interpreter line is one ``urllib_pack._PINNED`` certifies
  (drift here means a new Python line shipped: extend _PINNED and the
  REGISTRY-row comment together);
* patch/unpatch round-trips via the public ``install()``/``uninstall()``;
* the behavioral leg against FaultServer: a 503→200 GET is retried to
  success; a POST without an idempotency key is observed, not retried
  (KEEL-E014), and the original ``HTTPError`` re-raises unchanged.
"""

from __future__ import annotations

import inspect
import os
import platform
import unittest
import urllib.error
import urllib.request
from pathlib import Path
from tempfile import TemporaryDirectory

FARM = os.environ.get("KEEL_ADAPTER_FARM") == "1"
SKIP = "KEEL_ADAPTER_FARM=1 not set (offline fast path — see test_adapters_urllib.py)"


@unittest.skipUnless(FARM, SKIP)
class UrllibFarmTest(unittest.TestCase):
    def setUp(self) -> None:
        from keel import _runtime
        from keel._backend import load_backend
        from keel._defaults import level0_defaults
        from keel._discovery import Discovery
        from keel.adapters import urllib_pack

        self.urllib_pack = urllib_pack
        self._tmp = TemporaryDirectory()
        self.backend = load_backend("stub")
        self.backend.configure(level0_defaults())
        self.discovery = Discovery(Path(self._tmp.name))
        _runtime.set_runtime(self.backend, self.discovery)

    def tearDown(self) -> None:
        from keel import _runtime

        self.urllib_pack.uninstall()
        _runtime.clear_runtime()
        self.discovery.close()
        self._tmp.cleanup()

    def test_seam_signature_binds_as_documented(self) -> None:
        sig = inspect.signature(urllib.request.OpenerDirector.open)
        sig.bind(object(), "https://example.com", None, 1.0)  # (self, fullurl, data, timeout)

    def test_running_interpreter_is_a_certified_line(self) -> None:
        detection = self.urllib_pack.detect()
        self.assertTrue(detection.matched)
        self.assertEqual(detection.version, platform.python_version())
        self.assertEqual(
            detection.confidence,
            "pinned",
            f"Python {detection.version} is not in urllib_pack._PINNED — a new "
            "interpreter line shipped; certify it and extend _PINNED + the "
            "REGISTRY-row comment together.",
        )

    def test_patch_unpatch_round_trips(self) -> None:
        original = urllib.request.OpenerDirector.open
        self.urllib_pack.install()
        self.assertTrue(getattr(urllib.request.OpenerDirector.open, "__keel_wrapped__", False))
        self.urllib_pack.uninstall()
        self.assertIs(urllib.request.OpenerDirector.open, original)

    def test_behavioral_leg_retry_and_hard_rule(self) -> None:
        from .faultserver import FaultServer, fail, ok

        self.urllib_pack.install()
        with FaultServer([fail(503), ok(b"recovered")]) as srv:
            with urllib.request.urlopen(srv.url()) as r:
                self.assertEqual(r.read(), b"recovered")
                self.assertEqual(r.keel_outcome["attempts"], 2)
        with FaultServer([fail(503), ok(b"unreached")]) as srv:
            with self.assertRaises(urllib.error.HTTPError) as ctx:
                urllib.request.urlopen(srv.url(), data=b"body")
            self.assertEqual(ctx.exception.keel_outcome["error"]["code"], "KEEL-E014")
            self.assertEqual(srv.served, 1)


if __name__ == "__main__":
    unittest.main()
