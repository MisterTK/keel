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


def _run(script: str, policy: str, backend: str, timeout: int = 90, extra_env: dict | None = None):
    """Run `script` under `keel run` with `policy` as keel.toml and KEEL_BACKEND=backend."""
    with TemporaryDirectory() as d:
        Path(d, "keel.toml").write_text(textwrap.dedent(policy))
        Path(d, "prog.py").write_text(textwrap.dedent(script))
        env = {**os.environ, "KEEL_BACKEND": backend, "KEEL_QUIET": "1", **(extra_env or {})}
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


#: A slower per-request server than the shared `SERVER` (0.01s), used only by
#: `SyncConcurrencyTest`. With an 8-call baseline, `SERVER`'s 0.01s delay makes
#: fixed per-call overhead (connection setup, retry-wrapper bookkeeping, ~30ms
#: on this machine) a LARGE fraction of the total, which compresses the
#: serial-vs-threaded ratio towards 1 regardless of the delay itself and
#: leaves too little separation from a fixed cutoff to survive a loaded CI
#: runner (see the class docstring). At 0.15s/call the fixed overhead is a
#: small fraction of the total, so the ratio is dominated by whether the
#: calls actually overlap.
SLOW_SERVER = """
    import http.server, socketserver, threading, time, json, sys
    class H(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            time.sleep(0.15)
            self.send_response(200); self.end_headers(); self.wfile.write(b"ok")
        def log_message(self, *a): pass
    _srv = socketserver.ThreadingTCPServer(("127.0.0.1", 0), H)
    _srv.daemon_threads = True
    PORT = _srv.server_address[1]
    threading.Thread(target=_srv.serve_forever, daemon=True).start()
"""


class SyncConcurrencyTest(unittest.TestCase):
    """#116: independent sync effects must not serialize behind one another.

    A fixed wall-clock budget (the brief's original ">= 0.08s serialized /
    ~0.47s pre-fix") turned out not to be portable: on this machine, with the
    shared `SERVER`'s 0.01s per-call delay, 8 SERIALIZED calls finish in
    ~0.15s — so fixed per-call overhead (connection setup, retry-wrapper
    bookkeeping) is a large fraction of the total, and any absolute threshold
    loose enough to tolerate that overhead is also loose enough to pass with
    the #116 bug fully present. The first fix (self-calibrating serial-vs-
    threaded ratio instead of an absolute number) was correct in kind but
    still too thin in practice: measured pre-fix ratio was ~0.65-0.72 against
    a `< 0.6` cutoff, only ~0.1 of headroom — on a loaded CI runner a few ms
    of scheduling noise per call is a large fraction of an already-small
    ~0.15s baseline and can move the ratio across the line either way,
    silently passing pre-fix or failing post-fix.

    The fix here widens the separation at its source: a 0.15s per-call server
    delay (`SLOW_SERVER`, 15x `SERVER`'s) makes the fixed overhead a small
    fraction of the total, so the ratio is governed by whether the calls
    actually overlap, not by connection/bookkeeping noise. Re-measured on
    this machine (3 runs): serial ~1.35-1.37s, threaded ~1.26s, ratio
    ~0.925-0.935 — comfortable, reproducible separation from a `< 0.7` cutoff
    (about 0.23 of ratio headroom, versus ~0.1 before), and once #116's fix
    lands, 8 concurrent 0.15s calls should finish close to one call's cost
    (~0.15-0.2s), giving a ratio around 0.15 — nowhere near the boundary
    either direction.
    """

    PROG = SLOW_SERVER + """
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
        # 8 x 0.15s server-side (see SLOW_SERVER/class docstring for why not
        # SERVER's 0.01s). Pre-fix, measured ratio is ~0.93 (threaded barely
        # faster than fully serial) because the process-wide Mutex<Runtime>
        # serializes every sync effect; once concurrent, 8 calls should cost
        # close to ONE call, comfortably under 70% of the serial baseline.
        r = _run(self.PROG, self.POLICY, "native")
        self.assertEqual(r.returncode, 0, r.stderr.decode())
        got = json.loads(r.stdout.decode().strip().splitlines()[-1])
        serial, threaded = got["serial"], got["threaded"]
        self.assertLess(
            threaded, serial * 0.7,
            f"sync effects serialized: threaded={threaded:.3f}s serial={serial:.3f}s",
        )


class NestedEffectTest(unittest.TestCase):
    """#117: a nested sync effect must never hang. It completes, or it raises.

    The native path's real bound (`NESTED_EFFECT_WAIT` in
    `crates/keel-py/src/lib.rs`) is 30s in production — deliberately generous,
    since the detection mechanism (`try_lock` still failing) cannot tell "the
    outer effect dispatched to me" from "the outer effect is merely slow";
    the timeout's magnitude is the only thing separating a correct diagnosis
    from a false accusation against a legitimately slow concurrent caller
    (a Vertex/OpenAI generate call, a minute-scale poll deadline). This test's
    own budgets (`t.join`, the subprocess timeout below) are sized to fit
    THAT 30s constant with margin for the HTTP round-trip this reproduction
    needs on top of it — the test fits the product, not the other way round.
    `KEEL_NESTED_EFFECT_WAIT_MS` (unstable, test-only — see the constant's
    doc in lib.rs) shrinks the bound actually exercised here so the gate
    still runs fast without touching the production default.
    """

    #: Test-only override for `NESTED_EFFECT_WAIT` (native only; the stub has
    #: no such bound — it composes nesting fully). Comfortably above the
    #: 5ms poll interval and this reproduction's own network/thread overhead,
    #: comfortably below the ~45s budgets below.
    NATIVE_WAIT_OVERRIDE_MS = 2000

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
            out["body"] = httpx.get(f"http://127.0.0.1:{APP_PORT}/", timeout=40).text
        except Exception as e:
            out["error"] = f"{type(e).__name__}: {e}"
    # Sized to the REAL product bound (30s) plus this reproduction's own
    # HTTP-round-trip overhead, not to whatever made the test fast — the
    # native run overrides the bound down via KEEL_NESTED_EFFECT_WAIT_MS so
    # this rarely waits anywhere near 45s in practice, but the budget itself
    # must stay correct even if that override were absent.
    t = threading.Thread(target=outer); t.start(); t.join(timeout=45)
    print(json.dumps(out or {"hung": True}))
    """
    POLICY = """
    [target."127.0.0.1"]
    retry = { attempts = 3 }
    """

    def _result(self, backend: str, *, extra_env: dict | None = None) -> dict:
        r = _run(self.PROG, self.POLICY, backend, timeout=75, extra_env=extra_env)
        self.assertEqual(r.returncode, 0, r.stderr.decode())
        return json.loads(r.stdout.decode().strip().splitlines()[-1])

    def test_stub_composes_nested_effects(self) -> None:
        self.assertEqual(self._result("stub").get("body"), "ok")

    @unittest.skipUnless(_native_available(), "native core not built")
    def test_native_nested_effect_does_not_hang(self) -> None:
        # Outside a flow this must SUCCEED once #116 lands. Until then it
        # raises KEEL-E017 (this test's actual assertion), and this test
        # fails on a clean assertion rather than wedging the suite.
        got = self._result(
            "native",
            extra_env={"KEEL_NESTED_EFFECT_WAIT_MS": str(self.NATIVE_WAIT_OVERRIDE_MS)},
        )
        self.assertNotIn("hung", got, "nested sync effect hung — #117")
        self.assertEqual(got.get("body"), "ok", got)


class FlowOrderingTest(unittest.TestCase):
    """Guard rail for Task 4: making sync effects concurrent must NOT relax
    Tier 2's rule that steps inside one flow are admitted without overlap.

    The brief's original version called `step(0)..step(4)` sequentially from
    one thread, so `ORDER == [0,1,2,3,4]` BY CONSTRUCTION — there is no code
    path by which relaxing `active_flow`'s serialization could make it fail.
    It verified "a for-loop ran in order", not "flow steps are admitted
    without overlap under concurrent sync-effect dispatch".

    This version has 5 threads race to call `step(i)` (all `.start()`ed
    together, no `barrier.wait()` INSIDE the wrapped call — a barrier there
    deadlocks by construction: as long as steps genuinely serialize, only one
    thread is ever inside the wrapped effect at a time, so a barrier that
    needs all 5 threads to have entered can never release; this was verified
    empirically, not just reasoned about, while developing this fix). Each
    thread's `step(i)` sleeps for a distinct, DECREASING-with-`i` duration
    (`DELAYS[0]` is the longest, `DELAYS[4]` the shortest) purely with
    `time.sleep` — deliberately not a second wrapped effect, since a sync
    effect nested inside another (even same-thread: a plain `threading.Lock`
    is not reentrant, and neither is the native core's) is exactly #117's
    hang, reproduced while developing this test by nesting an `httpx` call
    inside `step`.

    Ground truth comes from the native journal itself (`steps_for_flow`,
    read from a fresh `KeelCore` opened read-only against the completed run's
    `.keel/journal.db` — safe because the writer process has already exited;
    this is NOT the live-journal-reader trap issue #14 documents), not from
    any front-end-recorded ordering: with concurrent racing threads, no
    front-end signal (e.g. a lock-protected append at the top of `step`) can
    be trusted to reflect the true order effects reached the native handle,
    since GIL scheduling can reorder Python-level bookkeeping independently
    of when a thread's underlying native call actually acquires the flow's
    lock. `steps_for_flow` avoids that problem entirely: it reports the
    real admission sequence and each step's actual `started_at`/`ended_at`.

    Two assertions follow directly from that ground truth, and together they
    ARE "admitted in call order, no duplicates, no drops":
      * the 5 effect steps' payloads (`step`'s return value, `i`) are exactly
        {0,1,2,3,4} — one each, none missing, none doubled;
      * sorted by `started_at`, no step's window overlaps the previous one's
        (`started_at[k] >= ended_at[k-1]`) — i.e. the flow admitted them ONE
        AT A TIME with no interleaving, which is the actual content of "in
        call order" for effects with no other externally observable identity
        (nothing here defines a "call order" independent of admission order;
        proving no overlap is the strongest, most direct statement of the
        Tier 2 guarantee obtainable from outside the native core).

    Confirmed pre-fix (today, everything still serializes): 3 runs measured
    elapsed ~1.53-1.6s against sum(DELAYS)=1.5s (not max(DELAYS)=0.5s), and
    the journal's `started_at`s were strictly increasing with zero overlap,
    in exactly call order 0,1,2,3,4 (GIL scheduled the racing threads'
    entries into the flow in the order they were `.start()`ed, on this
    machine) — i.e. today's real behavior already satisfies both assertions,
    as it must.

    How I convinced myself this would catch a Task 4 regression: if
    `active_flow`'s serialization were relaxed so that flow-scoped sync
    effects could run concurrently (the exact mistake Task 4 must avoid while
    making STANDALONE sync effects concurrent for #116), the 5 racing
    `time.sleep` calls would overlap in wall-clock time. Because DELAYS is
    strictly decreasing with `i`, true concurrency makes the SHORTEST calls
    (higher `i`) finish first regardless of which thread's call was admitted
    first — producing overlapping `[started_at, ended_at]` windows (caught by
    the non-overlap assertion) and a total elapsed near max(DELAYS)=0.5s
    instead of sum(DELAYS)=1.5s (an independent, journal-free confirmation
    from the subprocess's own wall clock). Both signals would move by a wide,
    unmissable margin (3x), not a fraction of a percent — this was checked by
    literally reproducing "no serialization" numbers via the earlier,
    now-superseded `SyncConcurrencyTest` design against a plain (non-flow)
    target, which showed exactly this max-vs-sum divergence.

    I could not find a way to observe "the order calls reached the handle"
    as a caller-independent fact without native journal introspection: any
    front-end-only signal (timestamps or lock-protected list appends taken
    in the calling threads) is itself subject to the same GIL-scheduling
    non-determinism as the calls it's trying to order, so it cannot serve as
    a ground truth distinct from what it's supposed to verify. Reading
    `steps_for_flow` after the process exits is the closest honest
    alternative: an authoritative, independent record of what the core
    actually admitted and when, not a front-end guess at it.
    """

    N = 5
    #: Deliberately DECREASING with i: under true concurrency, the shortest
    #: call (i=4) would finish first regardless of admission order, which is
    #: exactly what the non-overlap/elapsed-sum assertions below would catch.
    DELAYS = [0.5, 0.4, 0.3, 0.2, 0.1]

    PROG = """
    import threading, time, json
    DELAYS = %s
    def step(i):
        time.sleep(DELAYS[i])
        return i
    def main():
        threads = [threading.Thread(target=step, args=(i,)) for i in range(len(DELAYS))]
        t0 = time.perf_counter()
        for x in threads: x.start()
        for x in threads: x.join()
        elapsed = time.perf_counter() - t0
        print(json.dumps({"elapsed": elapsed}))
    if __name__ == "__main__":
        main()
    """ % (DELAYS,)
    POLICY = """
    [flows]
    entrypoints = ["py:prog:main"]

    [target."py:prog.step"]
    """
    ENTRYPOINT = "py:prog:main"

    def _run_and_read_journal(self) -> tuple[dict, list[dict]]:
        """Run the flow, then — with the writer process already exited, so
        this is a read of a closed journal, not the live-journal-reader trap
        issue #14 documents — open a fresh, read-only `KeelCore` against the
        SAME `.keel/journal.db` and pull the flow's real step history.
        `_run`'s own `TemporaryDirectory` is gone by the time it returns (its
        `with` block closes before the `return` completes), so this needs its
        own directory, kept alive across both the subprocess run and the
        journal read.
        """
        import keel_core

        d = TemporaryDirectory()
        try:
            dp = Path(d.name)
            dp.joinpath("keel.toml").write_text(textwrap.dedent(self.POLICY))
            dp.joinpath("prog.py").write_text(textwrap.dedent(self.PROG))
            env = {**os.environ, "KEEL_BACKEND": "native", "KEEL_QUIET": "1"}
            r = subprocess.run(
                [sys.executable, "-m", "keel", "run", "prog.py"],
                cwd=str(dp), env=env, capture_output=True, timeout=60,
            )
            self.assertEqual(r.returncode, 0, r.stderr.decode())
            result = json.loads(r.stdout.decode().strip().splitlines()[-1])
            core = keel_core.KeelCore(journal_path=str(dp / ".keel" / "journal.db"))
            flows = core.flows_by_entrypoint(self.ENTRYPOINT)
            self.assertEqual(len(flows), 1, flows)
            steps = core.steps_for_flow(flows[0]["flow_id"])
            return result, steps
        finally:
            d.cleanup()

    @unittest.skipUnless(_native_available(), "native core not built")
    def test_flow_steps_stay_in_call_order(self) -> None:
        result, steps = self._run_and_read_journal()
        effects = [s for s in steps if s["kind"] == "effect"]

        # No duplicates, no drops: exactly one step per index, in some order.
        self.assertEqual(
            sorted(s["payload"] for s in effects), list(range(self.N)),
            f"steps admitted: {effects}",
        )

        # No overlap: admitted and executed one at a time, never concurrently.
        by_start = sorted(effects, key=lambda s: s["started_at"])
        for prev, cur in zip(by_start, by_start[1:]):
            self.assertGreaterEqual(
                cur["started_at"], prev["ended_at"],
                f"flow steps overlapped: {prev} then {cur}",
            )

        # Independent, journal-free confirmation: total time tracks the SUM
        # of the per-step delays (serialized), not the MAX (concurrent) —
        # sum=1.5s vs max=0.5s, a 3x margin, from the subprocess's own clock.
        self.assertGreater(
            result["elapsed"], 0.8 * sum(self.DELAYS),
            f"flow steps ran concurrently: elapsed={result['elapsed']:.3f}s "
            f"sum(DELAYS)={sum(self.DELAYS):.3f}s",
        )


if __name__ == "__main__":
    unittest.main()
