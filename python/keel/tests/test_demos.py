"""Smoke tests that actually EXECUTE the demo scripts in `demos/` against
`tools/faultproxy`, so the demos can never silently rot. Reuses the shipped demo
files (app/scenario/keel.toml) rather than duplicating them.

  * flaky-python: bare app dies on the 503; `keel run` survives (Tier 1; stub OK).
  * agent-demo:   429 storm ridden out, then dev-cache replays across two native
                  runs with ~0 API calls (native-only; skips otherwise).
  * adk-demo:     a real google-adk LlmAgent's tool call rides out the same
                  storm below the agent loop (needs google-adk; skips otherwise).
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


if __name__ == "__main__":
    unittest.main()
