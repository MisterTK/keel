"""Sprint Team-B acceptance: the flaky-httpx demo, automated.

Same script, two runs against a 503-then-200 endpoint: bare it dies on the
503; under ``python -m keel run`` (Level 0 defaults retry 5xx) the transport
seam retries and the script succeeds — the zero-code-changes promise, proven
end to end in a child process."""

from __future__ import annotations

import subprocess
import sys
import unittest
from tempfile import TemporaryDirectory

from . import FIXTURES, child_env
from .faultserver import FaultServer, fail, ok

FLAKY = str(FIXTURES / "flaky_httpx_app.py")


class FlakyDemoTest(unittest.TestCase):
    def test_bare_run_dies_on_the_first_503(self) -> None:
        with FaultServer([fail(503), ok(b"late")]) as srv, TemporaryDirectory() as d:
            env = child_env(KEEL_DEMO_URL=srv.url("/flaky"))
            proc = subprocess.run([sys.executable, FLAKY], env=env, cwd=d, capture_output=True)
            self.assertNotEqual(proc.returncode, 0)
            self.assertNotIn(b"flaky ok", proc.stdout)
            self.assertEqual(srv.served, 1)  # one shot, then the process dies

    def test_keel_run_survives_the_flaky_endpoint(self) -> None:
        with FaultServer([fail(503), ok(b"late")]) as srv, TemporaryDirectory() as d:
            env = child_env(KEEL_DEMO_URL=srv.url("/flaky"), KEEL_QUIET="1")
            proc = subprocess.run(
                [sys.executable, "-m", "keel", "run", FLAKY],
                env=env,
                cwd=d,
                capture_output=True,
            )
            self.assertEqual(proc.returncode, 0, proc.stderr.decode())
            self.assertEqual(proc.stdout, b"flaky ok\n")
            self.assertEqual(srv.served, 2)  # 503 retried, then 200


if __name__ == "__main__":
    unittest.main()
