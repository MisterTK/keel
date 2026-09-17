"""Wall-clock, concurrency and nesting behavior of the effect path (#116/#117/#119).

These cannot live in conformance/: that suite is deterministic *because* it
runs on a virtual clock, so "did we really sleep" is unaskable there. They are
backend-specific behavior tests, so each one names the backend it asserts
against rather than assuming the ambient one.
"""

from __future__ import annotations

import json
import subprocess
import sys
import textwrap
import time
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

from keel_core_stub import KeelCoreStub

from . import child_env

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
        # `child_env` (NOT a raw `os.environ` copy): it is what injects
        # PYTHONPATH=<src>:<stub>:<fixtures>, without which the child cannot
        # `import keel` under the documented, PYTHONPATH-less command
        # (`cd python/keel && python3 -m unittest discover`) — i.e. under CI.
        # It also DROPS `KEEL_STUB_PAUSED`, which `tests/__init__` sets for the
        # in-process suite: these tests measure real time, so their children
        # must run the stub's real clock.
        env = child_env(KEEL_BACKEND=backend, KEEL_QUIET="1", **(extra_env or {}))
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
        # OUTSIDE a flow this SUCCEEDS since #116: the handle-wide
        # `Mutex<Runtime>` is gone and `active_flow` is unheld, so there is
        # nothing left for the inner call to contend and it simply runs.
        # (Before #116 it raised KEEL-E017, and before #117 it hung forever.)
        # The in-flow case, where `active_flow` IS held and KEEL-E017 is still
        # the correct answer, is `InFlowNestedEffectTest` below.
        got = self._result(
            "native",
            extra_env={"KEEL_NESTED_EFFECT_WAIT_MS": str(self.NATIVE_WAIT_OVERRIDE_MS)},
        )
        self.assertNotIn("hung", got, "nested sync effect hung — #117")
        self.assertEqual(got.get("body"), "ok", got)


class InFlowNestedEffectTest(unittest.TestCase):
    """KEEL-E017 (CCR-9) still fires where it is still the right answer.

    #116 deleted the handle-wide `Mutex<Runtime>`, which was one of the two
    locks that could expire `execute`'s bounded acquire — and with it the only
    way `NestedEffectTest` above could reach KEEL-E017, since that
    reproduction runs OUTSIDE a flow and now legitimately succeeds. A contract
    code nothing can raise is indistinguishable from a dead one, so this is
    the in-flow companion: `active_flow` is genuinely held for a step's whole
    duration (that IS Tier 2's admission rule, see `FlowOrderingTest`), so a
    second synchronous effect on another thread that cannot get it inside the
    bound is exactly the condition the code describes.

    The reproduction is contention on that lock rather than a literal nested
    dispatch, which is the mechanism itself: `try_lock` still failing is the
    ONLY signal available, and it cannot distinguish "the outer step
    dispatched to me" from "the outer step is merely slow" — which is why the
    production bound is 30s and why the message is worded as a possibility.
    Here `KEEL_NESTED_EFFECT_WAIT_MS` shrinks the bound below the outer step's
    duration so the expiry happens in ~1s instead of ~30s. 1000ms is the floor
    the override clamps to (a stray `=0` inherited by a production process
    would fail every contended in-flow effect instantly), so asking for less
    would silently get this anyway.

    Margin: the race thread's contended `inner()` attempt starts at 0.3s and
    the (floored) 1000ms bound expires it at ~1.3s; `OUTER_SECONDS` must clear
    that with real headroom on a loaded CI runner, which has never run this
    test. 3.0s gives ~1.7s of slack (vs. the original 2.0s's bare 0.7s) —
    comfortably larger without making the test slow. Removing the
    `KEEL_NESTED_EFFECT_WAIT_MS` override (production bound 30s) must still
    make this test fail: the outer step then releases the lock at 3.0s, well
    inside the 30s bound, so `inner()` succeeds instead of raising KEEL-E017 —
    proving this pins the expiry, not a constant.
    """

    WAIT_OVERRIDE_MS = 1000
    OUTER_SECONDS = 3.0

    PROG = """
    import threading, time, json
    def slow():
        time.sleep(%s)
        return "outer"
    def inner():
        return "inner"
    out = {}
    def race():
        # Let `slow` claim the flow's lock first; it holds it for the whole
        # step, i.e. well past this thread's shortened bound.
        time.sleep(0.3)
        try:
            out["inner"] = inner()
        except Exception as e:
            out["error"] = f"{type(e).__name__}: {e}"
    def main():
        t = threading.Thread(target=race)
        t.start()
        slow()
        t.join(timeout=30)
        print(json.dumps(out or {"hung": True}))
    if __name__ == "__main__":
        main()
    """ % (OUTER_SECONDS,)
    POLICY = """
    [flows]
    entrypoints = ["py:prog:main"]

    [target."py:prog.slow"]

    [target."py:prog.inner"]
    """

    @unittest.skipUnless(_native_available(), "native core not built")
    def test_native_in_flow_contended_effect_raises_e017(self) -> None:
        r = _run(
            self.PROG, self.POLICY, "native", timeout=75,
            extra_env={"KEEL_NESTED_EFFECT_WAIT_MS": str(self.WAIT_OVERRIDE_MS)},
        )
        self.assertEqual(r.returncode, 0, r.stderr.decode())
        got = json.loads(r.stdout.decode().strip().splitlines()[-1])
        self.assertNotIn("hung", got, "in-flow contended effect hung — the bound did not apply")
        self.assertIn("error", got, f"expected KEEL-E017, got {got}")
        self.assertIn("KEEL-E017", got["error"], got)


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
            env = child_env(KEEL_BACKEND="native", KEEL_QUIET="1")
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


class StubRealClockTest(unittest.TestCase):
    """#119, second half: the unpaused stub needs a real CLOCK, not just real
    sleeps.

    The first pass gave `_wait` a `time.sleep` but left `_now_ms` a counter
    that only `_wait`/`advance_clock` moved. Every layer that expires by
    comparing against `_now_ms` therefore could not expire on its own:

      * an OPEN BREAKER never closes — while open, calls fast-fail, a
        fast-fail performs no wait, no wait means no clock movement, so
        `_now_ms < open_until` stays true forever. A permanent,
        self-sustaining outage for that target, in a backend `KEEL_BACKEND=auto`
        silently selects in production.
      * a CACHE ENTRY's ttl never expires (same comparison).

    These run in-process against a directly constructed, explicitly UNPAUSED
    stub — the production configuration — with sub-second durations, so they
    cost ~1s of wall clock, not a schedule.
    """

    #: Long enough that a same-millisecond read can't pass by luck, short
    #: enough to keep the test near a second.
    COOLDOWN_MS = 400

    @staticmethod
    def _boom(_attempt: int) -> dict:
        return {"status": "error", "class": "server", "message": "boom"}

    @staticmethod
    def _fine(_attempt: int) -> dict:
        return {"status": "ok", "payload": {"v": 1}}

    @staticmethod
    def _req(**extra) -> dict:
        return {"v": 1, "target": "api.example.com", **extra}

    def test_open_breaker_closes_after_a_real_cooldown(self) -> None:
        core = KeelCoreStub(paused=False)
        core.configure(
            {
                "target": {
                    "api.example.com": {
                        "breaker": {"failures": 1, "cooldown": f"{self.COOLDOWN_MS}ms"},
                        "retry": {"attempts": 1},
                    }
                }
            }
        )
        first = core.execute(self._req(), self._boom)
        self.assertEqual(first["result"], "error", first)
        self.assertEqual(core.report()["targets"]["api.example.com"]["breaker_state"], "open")

        # Still open immediately: this pins that the clock isn't simply racing
        # ahead, i.e. the close below is the cooldown elapsing, not a no-op.
        blocked = core.execute(self._req(), self._fine)
        self.assertEqual(blocked["breaker"], "open", blocked)

        time.sleep(self.COOLDOWN_MS / 1000.0 + 0.2)
        # Pre-fix this hangs open forever: fast-failing calls never `_wait`, so
        # the counter that `open_until` is compared against never moves.
        self.assertEqual(
            core.report()["targets"]["api.example.com"]["breaker_state"],
            "closed",
            "an open breaker never closed on the unpaused stub — the clock is not real",
        )
        recovered = core.execute(self._req(), self._fine)
        self.assertEqual(recovered["result"], "ok", recovered)

    def test_cache_ttl_expires_on_real_time(self) -> None:
        core = KeelCoreStub(paused=False)
        core.configure({"target": {"api.example.com": {"cache": {"ttl": "400ms"}}}})
        req = self._req(args_hash="h1")
        self.assertFalse(core.execute(req, self._fine)["from_cache"])
        self.assertTrue(core.execute(req, self._fine)["from_cache"], "expected a cache hit")
        time.sleep(0.6)
        self.assertFalse(
            core.execute(req, self._fine)["from_cache"],
            "a cache entry outlived its ttl — the clock is not real",
        )

    def test_advance_clock_still_composes_when_unpaused(self) -> None:
        """`advance_clock` is an OFFSET on the real clock, not the clock
        itself: the paused harness still owns time, and an unpaused core can
        still be pushed forward without the two mechanisms fighting."""
        core = KeelCoreStub(paused=False)
        core.configure({"target": {"api.example.com": {"cache": {"ttl": "1h"}}}})
        req = self._req(args_hash="h2")
        core.execute(req, self._fine)
        self.assertTrue(core.execute(req, self._fine)["from_cache"])
        core.advance_clock(2 * 60 * 60 * 1000)
        self.assertFalse(core.execute(req, self._fine)["from_cache"])

    def test_paused_clock_is_unchanged(self) -> None:
        """The conformance contract: paused, time moves only on `_wait` and
        `advance_clock`, and starts at 0."""
        core = KeelCoreStub(paused=True)
        core.configure({"target": {"api.example.com": {"cache": {"ttl": "1s"}}}})
        self.assertEqual(core.report()["clock_ms"], 0)
        req = self._req(args_hash="h3")
        core.execute(req, self._fine)
        time.sleep(0.05)
        self.assertEqual(core.report()["clock_ms"], 0, "a paused clock moved with real time")
        self.assertTrue(core.execute(req, self._fine)["from_cache"])
        core.advance_clock(2000)
        self.assertFalse(core.execute(req, self._fine)["from_cache"])


class InEffectGuardTest(unittest.TestCase):
    """#120: `report()`/`enter_flow()`/`exit_flow()` refuse with `KEEL-E005`,
    and `recorded_idempotency_key()` degrades to `None`, when called from
    inside a synchronous effect — instead of the native core's undocumented
    `PanicException` (a `tokio::sync::Mutex::blocking_lock`/`Runtime::block_on`
    panic from within an already-running runtime context).
    `journal_time`/`journal_random` already had this `in_effect()` guard
    (the model this fix follows for `recorded_idempotency_key`);
    `report`/`enter_flow` had none at all — see `crates/keel-py/src/lib.rs`.
    `exit_flow`'s guard is defensive (its own docstring notes no known call
    site reaches it in this state today, transitively protected by
    `enter_flow`'s guard) rather than a reachable-today bug like the other
    two, but the mechanism is identical and this test exercises it the same
    way for the same reason: a documented landmine is still a landmine.

    The reproduction is a REAL nested effect, not a direct unit-test call
    against a bare `KeelCore`: `probe` is a `py:` function target, so by the
    time its body runs, the native core's `IN_EFFECT` thread-local is set
    (inside `invoke_sync_effect`, itself called from the effect closure
    `execute()` built for the `probe()` call) — calling `KeelCore` methods
    directly from inside that body is exactly the "wrapped `py:`/`tool:`
    effect" case issue #120 names, reached without needing a real `cmd:` rule
    or ADK Runner flow. `probe` is called from `main`, a `[flows]`
    entrypoint — needed only so `keel run` imports the script as a real
    module named `prog` (triggering the `py:` import hook, which wraps
    module-level functions only AFTER `exec_module` runs — `_hook.py`'s
    docstring), not because the flow itself matters to this test.
    """

    PROG = """
    import json

    def probe():
        from keel._runtime import get_backend
        backend = get_backend()
        out = {}
        try:
            backend.report()
            out["report"] = "no_error"
        except Exception as e:
            out["report"] = getattr(e, "code", type(e).__name__)
        try:
            backend.enter_flow("py:prog:probe", "h")
            out["enter_flow"] = "no_error"
        except Exception as e:
            out["enter_flow"] = getattr(e, "code", type(e).__name__)
        try:
            out["recorded_idempotency_key"] = backend.recorded_idempotency_key("t#-")
        except Exception as e:
            out["recorded_idempotency_key"] = f"raised:{getattr(e, 'code', type(e).__name__)}"
        try:
            backend.exit_flow("completed")
            out["exit_flow"] = "no_error"
        except Exception as e:
            out["exit_flow"] = getattr(e, "code", type(e).__name__)
        return out

    def main():
        print(json.dumps(probe()))

    if __name__ == "__main__":
        main()
    """
    POLICY = """
    [flows]
    entrypoints = ["py:prog:main"]

    [target."py:prog.probe"]
    """

    @unittest.skipUnless(_native_available(), "native core not built")
    def test_native_in_effect_calls_do_not_panic(self) -> None:
        r = _run(self.PROG, self.POLICY, "native")
        self.assertEqual(r.returncode, 0, r.stderr.decode())
        got = json.loads(r.stdout.decode().strip().splitlines()[-1])
        self.assertEqual(got["report"], "KEEL-E005", got)
        self.assertEqual(got["enter_flow"], "KEEL-E005", got)
        self.assertIsNone(got["recorded_idempotency_key"], got)
        self.assertEqual(got["exit_flow"], "KEEL-E005", got)
