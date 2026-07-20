"""Farm contract test: keel.adapters.subprocess_pack against the REAL stdlib.

Runs ONLY under KEEL_ADAPTER_FARM=1 (see test_farm_adk.py's module docs for the
full convention). Stdlib adaptation of the three-layer convention (mirrors
test_farm_urllib.py): there is no structural fake and no pip install —
``subprocess`` is always present, so the offline fast path
(tests/test_adapters_subprocess.py) already drives the real seam. This module is
the explicit certification ritual, certifying on the running interpreter (the
pack's version is keyed to the Python runtime — the documented stdlib convention
exception):

* ``subprocess.run`` / ``subprocess.call`` exist and their signatures bind the
  ``args``-first shape the wrappers re-declare, and ``check_output`` /
  ``check_call`` still dispatch through the module-global ``run`` / ``call``
  (the property that lets patching those two cover all four entrypoints);
* the running interpreter line is one ``subprocess_pack._PINNED`` certifies;
* patch/unpatch round-trips via the public ``install()``/``uninstall()``;
* the behavioral leg: a matched argv dispatches through the real seam to a real
  subprocess execution and the real result is returned (driven with a
  native-shaped flow-backend double, since Tier-2 needs a flow surface the stub
  lacks).
"""

from __future__ import annotations

import inspect
import os
import platform
import subprocess
import sys
import unittest

FARM = os.environ.get("KEEL_ADAPTER_FARM") == "1"
SKIP = "KEEL_ADAPTER_FARM=1 not set (offline fast path — see test_adapters_subprocess.py)"


@unittest.skipUnless(FARM, SKIP)
class SubprocessFarmTest(unittest.TestCase):
    def setUp(self) -> None:
        from keel import _runtime
        from keel.adapters import subprocess_pack

        from .test_adapters_subprocess import _FakeFlowBackend, _py, _rule

        self._runtime = _runtime
        self.subprocess_pack = subprocess_pack
        self._FakeFlowBackend = _FakeFlowBackend
        self._py = _py
        self._rule = _rule

    def tearDown(self) -> None:
        self.subprocess_pack.uninstall()
        self._runtime.clear_runtime()

    def test_seam_signatures_bind_as_documented(self) -> None:
        inspect.signature(subprocess.run).bind(["prog", "arg"])
        inspect.signature(subprocess.call).bind(["prog", "arg"])

    def test_check_wrappers_still_route_through_run_and_call(self) -> None:
        # The load-bearing coverage fact: check_output -> run, check_call -> call
        # (module-global name lookup), so patching run/call covers all four.
        self.assertIn("run(", inspect.getsource(subprocess.check_output))
        self.assertIn("call(", inspect.getsource(subprocess.check_call))

    def test_running_interpreter_is_a_certified_line(self) -> None:
        detection = self.subprocess_pack.detect()
        self.assertTrue(detection.matched)
        self.assertEqual(detection.version, platform.python_version())
        self.assertEqual(
            detection.confidence,
            "pinned",
            f"Python {detection.version} is not in subprocess_pack._PINNED — a new "
            "interpreter line shipped; certify it and extend _PINNED together.",
        )

    def test_patch_unpatch_round_trips(self) -> None:
        orig_run, orig_call = subprocess.run, subprocess.call
        self.subprocess_pack.install()
        self.assertTrue(getattr(subprocess.run, "__keel_wrapped__", False))
        self.assertTrue(getattr(subprocess.call, "__keel_wrapped__", False))
        self.subprocess_pack.uninstall()
        self.assertIs(subprocess.run, orig_run)
        self.assertIs(subprocess.call, orig_call)

    def test_behavioral_leg_matched_argv_dispatches_to_real_execution(self) -> None:
        backend = self._FakeFlowBackend()
        self._runtime.set_runtime(backend, None)
        self._runtime.set_cmd_flows(self._rule("cmd:x", ["*", "-c", "*"]))
        self.subprocess_pack.install()
        result = subprocess.run(self._py("print('farm')"), capture_output=True, text=True)
        self.assertEqual(result.stdout.strip(), "farm")
        self.assertEqual(len(backend.entered), 1, "the matched argv entered a cmd flow")
        self.assertEqual(backend.exited, ["completed"])


if __name__ == "__main__":
    unittest.main()
