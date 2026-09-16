import threading
import unittest

from keel._cachepoll import MAX_FIRED, MAX_RUNS, CachePollDetector

HIT = {"v": 1, "result": "ok", "attempts": 0, "from_cache": True}
MISS = {"v": 1, "result": "ok", "attempts": 1, "from_cache": False}


class FakeClock:
    def __init__(self) -> None:
        self.t = 0.0

    def __call__(self) -> float:
        return self.t


class CachePollDetectorTest(unittest.TestCase):
    def setUp(self) -> None:
        self.clock = FakeClock()
        self.fired = []
        self.d = CachePollDetector(
            clock=self.clock, on_suspect=lambda t, h, s: self.fired.append((t, h, s))
        )

    def _hits(self, n: int, step: float, key: str = "h1") -> None:
        for _ in range(n):
            self.d.observe("llm:google-genai", key, HIT)
            self.clock.t += step

    def test_five_regular_hits_over_twenty_seconds_fire_once(self) -> None:
        self._hits(5, 10.0)
        self.assertEqual(self.fired, [("llm:google-genai", 5, 40)])
        self._hits(20, 10.0)
        self.assertEqual(len(self.fired), 1, "once per key per process")

    def test_a_burst_does_not_fire(self) -> None:
        self._hits(50, 0.01)  # a test suite replaying one prompt
        self.assertEqual(self.fired, [])

    def test_a_real_fetch_resets_the_run(self) -> None:
        self._hits(4, 10.0)
        self.d.observe("llm:google-genai", "h1", MISS)
        self._hits(4, 10.0)
        self.assertEqual(self.fired, [], "never five consecutive")

    def test_keys_are_independent_and_none_is_ignored(self) -> None:
        for i in range(10):
            self.d.observe("llm:openai", f"h{i}", HIT)
            self.clock.t += 10.0
        self.assertEqual(self.fired, [], "different prompts, not a poll")
        for _ in range(10):
            self.d.observe("llm:openai", None, HIT)
            self.clock.t += 10.0
        self.assertEqual(self.fired, [])

    def test_concurrent_hits_on_one_key_fire_exactly_once_and_lose_no_hits(self) -> None:
        # Real threads, the real clock, min_span_s=0 so firing depends only
        # on hit count (not on real wall-clock spacing) — isolates the race
        # on `_runs`/`_fired` from timing noise.
        fired: list[tuple[str, int, int]] = []
        d = CachePollDetector(min_span_s=0.0, on_suspect=lambda t, h, s: fired.append((t, h, s)))
        n_threads = 32
        hits_per_thread = 200
        barrier = threading.Barrier(n_threads)

        def worker() -> None:
            barrier.wait()
            for _ in range(hits_per_thread):
                d.observe("llm:google-genai", "h1", HIT)

        threads = [threading.Thread(target=worker) for _ in range(n_threads)]
        for t in threads:
            t.start()
        for t in threads:
            t.join()

        self.assertEqual(len(fired), 1, "fires exactly once under concurrency")
        total_hits = n_threads * hits_per_thread
        run = d._runs[("llm:google-genai", "h1")]
        self.assertEqual(run[0], total_hits, "no hit lost to the read-modify-write race")

    def test_eviction_caps_runs_and_an_active_run_near_the_cap_still_fires(self) -> None:
        clock = FakeClock()
        fired = []
        d = CachePollDetector(clock=clock, on_suspect=lambda t, h, s: fired.append((t, h, s)))
        for i in range(MAX_RUNS + 50):
            d.observe("llm:openai", f"h{i}", HIT)
            clock.t += 0.001
        self.assertLessEqual(len(d._runs), MAX_RUNS)
        # An active run, touched most recently (so never the oldest-`last`
        # eviction candidate), must survive the churn above and still fire.
        for _ in range(5):
            d.observe("llm:google-genai", "active", HIT)
            clock.t += 10.0
        self.assertEqual(fired, [("llm:google-genai", 5, 40)])

    def test_fired_set_eviction_caps_growth(self) -> None:
        clock = FakeClock()
        d = CachePollDetector(clock=clock, min_hits=1, min_span_s=0.0)
        for i in range(MAX_FIRED + 50):
            d.observe("llm:openai", f"f{i}", HIT)
            d.observe("llm:openai", f"f{i}", HIT)  # second hit fires (min_hits=1, span=0)
        self.assertLessEqual(len(d._fired), MAX_FIRED)


if __name__ == "__main__":
    unittest.main()
