"""Wall-clock, concurrency and nesting behavior of the effect path (#116/#117/#119).

These cannot live in conformance/: that suite is deterministic *because* it
runs on a virtual clock, so "did we really sleep" is unaskable there. They are
backend-specific behavior tests, so each one names the backend it asserts
against rather than assuming the ambient one.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
import textwrap
import time
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

NATIVE = None


def _native_available() -> bool:
    global NATIVE
    if NATIVE is None:
        try:
            import keel_core  # noqa: F401

            NATIVE = True
        except Exception:
            NATIVE = False
    return NATIVE


def _run(script: str, policy: str, backend: str, timeout: int = 90):
    """Run `script` under `keel run` with `policy` as keel.toml and KEEL_BACKEND=backend."""
    with TemporaryDirectory() as d:
        Path(d, "keel.toml").write_text(textwrap.dedent(policy))
        Path(d, "prog.py").write_text(textwrap.dedent(script))
        env = {**os.environ, "KEEL_BACKEND": backend, "KEEL_QUIET": "1"}
        return subprocess.run(
            [sys.executable, "-m", "keel", "run", "prog.py"],
            cwd=d, env=env, capture_output=True, timeout=timeout,
        )


SERVER = """
    import http.server, socketserver, threading, time, json, sys
    class H(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            time.sleep(2.0 if self.path.startswith("/slow") else 0.01)
            self.send_response(200); self.end_headers(); self.wfile.write(b"ok")
        def log_message(self, *a): pass
    _srv = socketserver.ThreadingTCPServer(("127.0.0.1", 0), H)
    _srv.daemon_threads = True
    PORT = _srv.server_address[1]
    threading.Thread(target=_srv.serve_forever, daemon=True).start()
"""


class WallClockTest(unittest.TestCase):
    """#119: a policy's durations must cost real time on EVERY backend."""

    RATE_PROG = SERVER + """
    import httpx, time
    t = time.perf_counter()
    for _ in range(40):
        httpx.get(f"http://127.0.0.1:{PORT}/", timeout=30)
    print(json.dumps({"elapsed": time.perf_counter() - t}))
    """
    RATE_POLICY = """
    [target."127.0.0.1"]
    rate = "30/min"
    """

    def _elapsed(self, backend: str) -> float:
        r = _run(self.RATE_PROG, self.RATE_POLICY, backend)
        self.assertEqual(r.returncode, 0, r.stderr.decode())
        return json.loads(r.stdout.decode().strip().splitlines()[-1])["elapsed"]

    def test_stub_rate_limit_costs_real_time(self) -> None:
        # 40 calls at 30/min: the 10 over budget must wait out a real window.
        # Pre-fix the stub advances a counter and finishes in ~0.15s.
        self.assertGreater(self._elapsed("stub"), 15.0)

    @unittest.skipUnless(_native_available(), "native core not built")
    def test_native_rate_limit_costs_real_time(self) -> None:
        self.assertGreater(self._elapsed("native"), 15.0)


class SyncConcurrencyTest(unittest.TestCase):
    """#116: independent sync effects must not serialize behind one another.

    A fixed wall-clock budget (the brief's original ">= 0.08s serialized /
    ~0.47s pre-fix") turned out not to be portable: on this machine the
    per-call overhead is small enough that 8 SERIALIZED calls finish in
    ~0.15s, comfortably under a loose absolute threshold, which would pass
    this test even with the #116 bug present. Instead the program measures
    its OWN serial baseline (8 calls back-to-back) in the same process/host
    conditions immediately before the threaded run, and the test asserts the
    threaded run is meaningfully faster than that baseline — a self-
    calibrating comparison that isn't sensitive to absolute machine speed.
    """

    PROG = SERVER + """
    import httpx, threading, time
    def hit(): httpx.get(f"http://127.0.0.1:{PORT}/fast", timeout=30)
    t0 = time.perf_counter()
    for _ in range(8):
        hit()
    serial = time.perf_counter() - t0
    ths = [threading.Thread(target=hit) for _ in range(8)]
    t1 = time.perf_counter()
    for x in ths: x.start()
    for x in ths: x.join()
    threaded = time.perf_counter() - t1
    print(json.dumps({"serial": serial, "threaded": threaded}))
    """
    POLICY = """
    [target."127.0.0.1"]
    retry = { attempts = 3 }
    """

    @unittest.skipUnless(_native_available(), "native core not built")
    def test_native_sync_effects_run_concurrently(self) -> None:
        # 8 x ~10ms server-side. Pre-fix, the threaded run costs about as much
        # as the serial baseline (measured ratio ~0.72 on this machine) because
        # the process-wide Mutex<Runtime> serializes every sync effect; once
        # concurrent, 8 calls should finish close to the cost of ONE call, well
        # under 60% of the serial baseline.
        r = _run(self.PROG, self.POLICY, "native")
        self.assertEqual(r.returncode, 0, r.stderr.decode())
        got = json.loads(r.stdout.decode().strip().splitlines()[-1])
        serial, threaded = got["serial"], got["threaded"]
        self.assertLess(
            threaded, serial * 0.6,
            f"sync effects serialized: threaded={threaded:.3f}s serial={serial:.3f}s",
        )


class NestedEffectTest(unittest.TestCase):
    """#117: a nested sync effect must never hang. It completes, or it raises."""

    PROG = SERVER + """
    import httpx, threading
    class App(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            r = httpx.get(f"http://127.0.0.1:{PORT}/", timeout=10)   # INNER effect
            self.send_response(200); self.end_headers(); self.wfile.write(r.content)
        def log_message(self, *a): pass
    _app = socketserver.ThreadingTCPServer(("127.0.0.1", 0), App)
    _app.daemon_threads = True
    APP_PORT = _app.server_address[1]
    threading.Thread(target=_app.serve_forever, daemon=True).start()
    out = {}
    def outer():
        try:
            out["body"] = httpx.get(f"http://127.0.0.1:{APP_PORT}/", timeout=15).text
        except Exception as e:
            out["error"] = f"{type(e).__name__}: {e}"
    t = threading.Thread(target=outer); t.start(); t.join(timeout=25)
    print(json.dumps(out or {"hung": True}))
    """
    POLICY = """
    [target."127.0.0.1"]
    retry = { attempts = 3 }
    """

    def _result(self, backend: str) -> dict:
        r = _run(self.PROG, self.POLICY, backend, timeout=90)
        self.assertEqual(r.returncode, 0, r.stderr.decode())
        return json.loads(r.stdout.decode().strip().splitlines()[-1])

    def test_stub_composes_nested_effects(self) -> None:
        self.assertEqual(self._result("stub").get("body"), "ok")

    @unittest.skipUnless(_native_available(), "native core not built")
    def test_native_nested_effect_does_not_hang(self) -> None:
        # Outside a flow this must SUCCEED once #116 lands. Until then it hangs,
        # and this test fails by timeout rather than wedging the suite.
        got = self._result("native")
        self.assertNotIn("hung", got, "nested sync effect hung — #117")
        self.assertEqual(got.get("body"), "ok", got)


class FlowOrderingTest(unittest.TestCase):
    """Guard rail for Task 4: making sync effects concurrent must NOT relax
    Tier 2's rule that steps inside one flow are admitted in call order."""

    PROG = """
    import json
    ORDER = []
    def step(i):
        ORDER.append(i)
        return i
    def main():
        for i in range(5):
            step(i)
        print(json.dumps({"order": ORDER}))
    if __name__ == "__main__":
        main()
    """
    POLICY = """
    [flows]
    entrypoints = ["py:prog:main"]

    [target."py:prog.step"]
    """

    @unittest.skipUnless(_native_available(), "native core not built")
    def test_flow_steps_stay_in_call_order(self) -> None:
        r = _run(self.PROG, self.POLICY, "native")
        self.assertEqual(r.returncode, 0, r.stderr.decode())
        got = json.loads(r.stdout.decode().strip().splitlines()[-1])
        self.assertEqual(got["order"], [0, 1, 2, 3, 4])


if __name__ == "__main__":
    unittest.main()
