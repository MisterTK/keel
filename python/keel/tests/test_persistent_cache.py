"""Task 14 item 1 (load-bearing): cross-RUN dev-cache replay under the native
core + attached journal.

Two SEPARATE ``python -m keel run`` processes hit the same endpoint with an
identical request. Off-prod, the dev cache resolves to ``scope = "persistent"``
(native + journal at ``.keel/journal.db``), so the SECOND run replays from the
journal and makes ZERO API calls for the repeated prompt — the "repeated run
costs ~0" promise, proven end to end across processes.

Skips when the native ``keel_core`` module is not importable (build it with
``maturin develop`` in ``crates/keel-py``) — the persistent scope is native-only.
"""

from __future__ import annotations

import subprocess
import sys
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

from . import child_env
from .faultserver import FaultServer, ok

try:  # native-only: the persistent scope needs the real core + journal
    import keel_core  # noqa: F401

    _NATIVE = True
except ImportError:
    _NATIVE = False

_JSON = {"content-type": "application/json"}


@unittest.skipUnless(_NATIVE, "keel_core native module not built (maturin develop in crates/keel-py)")
class PersistentDevCacheTest(unittest.TestCase):
    def test_identical_prompt_replays_across_two_native_runs(self) -> None:
        # One scripted OK; a correct 2nd run makes NO request, so the queue is
        # never exhausted and `served` stays at 1.
        with FaultServer([ok(b'{"reply":"hi"}', _JSON)]) as srv, TemporaryDirectory() as d:
            Path(d, "keel.toml").write_text(
                '[target."127.0.0.1"]\ncache = { mode = "dev" }\nretry = { attempts = 1 }\n'
            )
            Path(d, "app.py").write_text(
                "import os, sys, httpx\n"
                "r = httpx.get(os.environ['KEEL_DEMO_URL'], timeout=5.0)\n"
                "hit = bool(getattr(r, 'keel_outcome', {}).get('from_cache'))\n"
                "sys.stdout.write('BODY:' + r.text + ' CACHE:' + str(hit) + '\\n')\n"
            )

            def run_once() -> subprocess.CompletedProcess[bytes]:
                env = child_env(  # child_env strips KEEL_BACKEND → auto → native
                    KEEL_DEMO_URL=srv.url("/v1/chat/completions"),
                    KEEL_QUIET="1",
                    KEEL_ENV="",  # off-prod so the dev cache is active
                )
                return subprocess.run(
                    [sys.executable, "-m", "keel", "run", "app.py"],
                    env=env,
                    cwd=d,
                    capture_output=True,
                )

            run1 = run_once()
            self.assertEqual(run1.returncode, 0, run1.stderr.decode())
            self.assertEqual(run1.stdout, b'BODY:{"reply":"hi"} CACHE:False\n', run1.stderr.decode())
            self.assertTrue(Path(d, ".keel", "journal.db").exists(), "journal.db written on run1")
            self.assertEqual(srv.served, 1, "run1 makes exactly one API call")

            run2 = run_once()
            self.assertEqual(run2.returncode, 0, run2.stderr.decode())
            # Same body, served from the persistent journal: from_cache=True and
            # NO new API call — a repeated prompt costs ~0 across runs.
            self.assertEqual(run2.stdout, b'BODY:{"reply":"hi"} CACHE:True\n', run2.stderr.decode())
            self.assertEqual(srv.served, 1, "run2 replays from the persistent cache — 0 API calls")


if __name__ == "__main__":
    unittest.main()
