import unittest

from keel._cachepoll import CachePollDetector

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


if __name__ == "__main__":
    unittest.main()
