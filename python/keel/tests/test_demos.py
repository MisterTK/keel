"""Smoke tests that actually EXECUTE the demo scripts in `demos/` against
`tools/faultproxy`, so the demos can never silently rot. Reuses the shipped demo
files (app/scenario/keel.toml) rather than duplicating them.

  * flaky-python: bare app dies on the 503; `keel run` survives (Tier 1; stub OK).
  * agent-demo:   429 storm ridden out, then dev-cache replays across two native
                  runs with ~0 API calls (native-only; skips otherwise).
  * adk-demo:     a real google-adk LlmAgent's tool call rides out the same
                  storm below the agent loop (needs google-adk; skips otherwise).
  * lro-poll:     a poll block without `until.absent` does not poll a running
                  google.longrunning.Operation; adding `absent = "pending"`
                  makes the same code poll to terminal (Tier 1; stub OK).
"""

from __future__ import annotations

import json
import subprocess
import sys
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

from . import REPO_ROOT, child_env

sys.path.insert(0, str(REPO_ROOT / "tools" / "faultproxy"))
from faultproxy import FaultProxy, Scenario  # noqa: E402

_DEMOS = REPO_ROOT / "demos"

try:
    import keel_core  # noqa: F401

    _NATIVE = True
except ImportError:
    _NATIVE = False

try:
    import google.adk  # noqa: F401

    _HAS_ADK = True
except ImportError:
    _HAS_ADK = False


def _scenario(demo: str) -> Scenario:
    return Scenario(json.loads((_DEMOS / demo / "scenario.json").read_text()))


class FlakyDemoRunTest(unittest.TestCase):
    def test_bare_dies_and_keel_run_survives(self) -> None:
        app = str(_DEMOS / "flaky-python" / "app.py")
        with FaultProxy(_scenario("flaky-python")) as proxy, TemporaryDirectory() as d:
            url = proxy.url("/flaky")
            bare = subprocess.run(
                [sys.executable, app],
                env=child_env(KEEL_DEMO_URL=url),
                cwd=d,
                capture_output=True,
            )
            self.assertNotEqual(bare.returncode, 0, "bare script must die on the 503")

            proxy.scenario.reset()  # rewind so `keel run` sees 503-then-200 again
            keeled = subprocess.run(
                [sys.executable, "-m", "keel", "run", app],
                env=child_env(KEEL_DEMO_URL=url, KEEL_QUIET="1"),
                cwd=d,
                capture_output=True,
            )
            self.assertEqual(keeled.returncode, 0, keeled.stderr.decode())
            self.assertEqual(keeled.stdout, b"flaky ok\n")


@unittest.skipUnless(_NATIVE, "keel_core native module not built (maturin develop in crates/keel-py)")
class AgentDemoDevCacheTest(unittest.TestCase):
    def test_429_storm_then_devcache_replay_across_runs(self) -> None:
        demo = _DEMOS / "agent-demo"
        with FaultProxy(_scenario("agent-demo")) as proxy, TemporaryDirectory() as d:
            Path(d, "keel.toml").write_text((demo / "keel.toml").read_text())
            url = proxy.url("/v1/complete")

            def run_once() -> subprocess.CompletedProcess[bytes]:
                return subprocess.run(
                    [sys.executable, "-m", "keel", "run", str(demo / "agent.py")],
                    env=child_env(KEEL_DEMO_URL=url, KEEL_ENV="", KEEL_QUIET="1"),
                    cwd=d,
                    capture_output=True,
                )

            run1 = run_once()
            self.assertEqual(run1.returncode, 0, run1.stderr.decode())
            self.assertEqual(run1.stdout, b"reply=42 from_cache=False\n", run1.stderr.decode())
            self.assertEqual(len(proxy.log), 3, "run 1: 2x429 storm + 1x200")

            run2 = run_once()
            self.assertEqual(run2.returncode, 0, run2.stderr.decode())
            self.assertEqual(run2.stdout, b"reply=42 from_cache=True\n", run2.stderr.decode())
            self.assertEqual(len(proxy.log), 3, "run 2 replayed from the dev cache — 0 new calls")


@unittest.skipUnless(_HAS_ADK, "google-adk not installed (farm leg covers this)")
class AdkDemoTest(unittest.TestCase):
    def test_429_storm_survives_below_the_agent_loop(self) -> None:
        demo = _DEMOS / "adk-demo"
        with FaultProxy(_scenario("adk-demo")) as proxy, TemporaryDirectory() as d:
            Path(d, "keel.toml").write_text((demo / "keel.toml").read_text())
            url = proxy.url("/v1/complete")

            result = subprocess.run(
                [sys.executable, "-m", "keel", "run", str(demo / "agent.py")],
                env=child_env(KEEL_DEMO_URL=url, KEEL_QUIET="1"),
                cwd=d,
                capture_output=True,
            )
            self.assertEqual(result.returncode, 0, result.stderr.decode())
            self.assertEqual(result.stdout, b"reply=42\n", result.stderr.decode())
            self.assertEqual(len(proxy.log), 3, "one agent turn absorbed a 2x429 + 1x200 storm")


class LroPollAbsentTest(unittest.TestCase):
    """The demo's CLAIM, not merely its exit code: the pre-CCR-11 poll block
    makes exactly ONE upstream GET and hands the app a still-running
    `google.longrunning.Operation` (no `done` key at all — proto3 JSON omits a
    false bool), while the SAME app.py under the SAME block plus
    `absent = "pending"` makes four and reaches the terminal body.

    Runs on whichever backend is present; poll is Tier 1, so both must agree.
    """

    def _run(self, policy: str, url: str) -> tuple[subprocess.CompletedProcess[bytes], str]:
        demo = _DEMOS / "lro-poll"
        with TemporaryDirectory() as d:
            Path(d, "keel.toml").write_text((demo / policy).read_text())
            proc = subprocess.run(
                [sys.executable, "-m", "keel", "run", str(demo / "app.py")],
                env=child_env(KEEL_DEMO_URL=url, KEEL_QUIET="1"),
                cwd=d,
                capture_output=True,
            )
        return proc, proc.stdout.decode()

    def test_absent_is_the_difference_between_not_polling_and_polling(self) -> None:
        demo = _DEMOS / "lro-poll"
        # The two policies must differ by exactly one line — the demo's whole
        # claim is "same code, one added key".
        without = (demo / "keel.without-absent.toml").read_text().splitlines()
        with_ = (demo / "keel.with-absent.toml").read_text().splitlines()
        self.assertEqual(len(without), len(with_))
        differing = [i for i, (a, b) in enumerate(zip(without, with_)) if a != b]
        self.assertEqual(len(differing), 1, "policies must differ by one line only")
        self.assertNotIn('absent = "pending"', without[differing[0]])
        self.assertIn('absent = "pending"', with_[differing[0]])

        with FaultProxy(_scenario("lro-poll")) as proxy:
            url = proxy.url("/v1/projects/demo/locations/us-central1/operations/123")

            # Act 1: fails open on the missing `done`, returns the running body.
            act1, out1 = self._run("keel.without-absent.toml", url)
            self.assertNotEqual(act1.returncode, 0, "the app must break on a non-terminal body")
            self.assertIn("done=None upstream_attempts=1", out1, act1.stderr.decode())
            self.assertNotIn('"done"', out1, "the running LRO body must not carry `done` at all")
            self.assertEqual(len(proxy.log), 1, "act 1 did not poll: one upstream GET")

            proxy.scenario.reset()  # rewind so act 2 sees the same body sequence

            # Act 2: same app.py, same block + `absent = "pending"`.
            act2, out2 = self._run("keel.with-absent.toml", url)
            self.assertEqual(act2.returncode, 0, act2.stderr.decode())
            self.assertIn("done=True upstream_attempts=4", out2, act2.stderr.decode())
            self.assertIn("video: gs://out/video.mp4", out2)
            self.assertEqual(len(proxy.log), 5, "act 2 polled: 3 pending + 1 terminal, after act 1's 1")


if __name__ == "__main__":
    unittest.main()
