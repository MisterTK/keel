"""The demo that sells the product (dx-spec §1 Level 2): a durable pipeline that
survives `kill -9` and resumes without re-firing completed steps.

Orchestrated as real subprocesses (the only honest way to test crash recovery):
  1. Run a 10-step flow that hard-crashes (SIGKILL) right before step 6. Each
     step appends one line to a shared side-effect log, so the log is a
     server-side-style invocation count no in-process mock could fake across a
     `kill -9`.
  2. After the lease expires, re-run the same `keel run`. Steps 1–5 are
     substituted from the journal (their effects never re-fire — the log gains
     no duplicate lines); steps 6–10 run live. The flow completes.
  3. `keel flows` shows the flow `completed` with 10/10 steps.

Requires the native core (Tier 2 is native-only); skips cleanly without it.
"""

from __future__ import annotations

import json
import signal
import sqlite3
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

from . import child_env

try:
    import keel_core  # noqa: F401

    _NATIVE = True
except ImportError:
    _NATIVE = False

_REPO = Path(__file__).resolve().parents[3]

# A 10-step pipeline. `do_step` is a wrapped `py:` target (zero user changes to
# make it durable): each call is a journaled effect that appends to the shared
# log. `KEEL_DEMO_CRASH_AT=n` self-SIGKILLs right before step n fires.
_PIPELINE = '''\
import os
import signal

_LOG = os.environ["KEEL_DEMO_LOG"]
_CRASH_AT = int(os.environ.get("KEEL_DEMO_CRASH_AT", "0"))


def do_step(n):
    with open(_LOG, "a", encoding="utf-8") as f:
        f.write(f"step-{n}\\n")
    return {"step": n}


def main():
    for n in range(1, 11):
        if _CRASH_AT and n == _CRASH_AT:
            os.kill(os.getpid(), signal.SIGKILL)  # kill -9 before step n fires
        do_step(n)
    print("PIPELINE_COMPLETE")
'''

_KEEL_TOML = '[flows]\nentrypoints = ["py:pipeline:main"]\n\n[target."py:pipeline.do_step"]\n'


def _keel_binary() -> str | None:
    """The built `keel` CLI, if present (so the demo can assert `keel flows`)."""
    for candidate in (_REPO / "target" / "debug" / "keel", _REPO / "target" / "release" / "keel"):
        if candidate.exists():
            return str(candidate)
    return None


def _log_steps(log: Path) -> list[str]:
    if not log.exists():
        return []
    return [line for line in log.read_text().splitlines() if line]


def _flow_status(journal: Path) -> tuple[str, int]:
    conn = sqlite3.connect(journal)
    try:
        status = conn.execute("SELECT status FROM flows").fetchone()[0]
        steps = conn.execute("SELECT COUNT(*) FROM steps WHERE kind != 'marker'").fetchone()[0]
        return status, steps
    finally:
        conn.close()


@unittest.skipUnless(_NATIVE, "keel_core native module not built (maturin develop in crates/keel-py)")
class ResumeDemoTest(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self._tmp.name)
        (self.dir / "pipeline.py").write_text(_PIPELINE)
        (self.dir / "keel.toml").write_text(_KEEL_TOML)
        self.log = self.dir / "effects.log"
        self.addCleanup(self._tmp.cleanup)

    def _run(self, *, crash_at: int | None = None) -> subprocess.CompletedProcess:
        extra = {
            "KEEL_DEMO_LOG": str(self.log),
            "KEEL_FLOW_LEASE_MS": "800",  # short lease so the demo resumes fast
            "KEEL_QUIET": "1",
            "KEEL_BACKEND": "native",
        }
        if crash_at is not None:
            extra["KEEL_DEMO_CRASH_AT"] = str(crash_at)
        env = child_env(**extra)
        return subprocess.run(
            [sys.executable, "-m", "keel", "run", "pipeline.py"],
            cwd=self.dir,
            env=env,
            capture_output=True,
            text=True,
        )

    def test_kill9_then_resume_completes_without_refiring(self) -> None:
        # Run 1: crash (kill -9) right before step 6.
        run1 = self._run(crash_at=6)
        self.assertEqual(
            run1.returncode, -signal.SIGKILL, f"expected SIGKILL; stderr={run1.stderr}"
        )
        self.assertEqual(
            _log_steps(self.log),
            [f"step-{n}" for n in range(1, 6)],
            "run 1 fired exactly steps 1-5 before the crash",
        )

        # Let run 1's lease expire so the resume can steal it.
        time.sleep(1.5)

        # Run 2: resume. Steps 1-5 are substituted (no new log lines); 6-10 fire.
        run2 = self._run()
        self.assertEqual(run2.returncode, 0, f"resume failed; stderr={run2.stderr}")
        self.assertIn("PIPELINE_COMPLETE", run2.stdout)
        self.assertEqual(
            _log_steps(self.log),
            [f"step-{n}" for n in range(1, 11)],
            "each step fired EXACTLY ONCE across both runs — 1-5 substituted on resume",
        )

        # The journal (what `keel flows` reads) shows the flow completed, 10 steps.
        status, steps = _flow_status(self.dir / ".keel" / "journal.db")
        self.assertEqual(status, "completed")
        self.assertEqual(steps, 10)

        # And `keel flows` itself, when the CLI is built.
        keel = _keel_binary()
        if keel is not None:
            out = subprocess.run(
                [keel, "--json", "flows"], cwd=self.dir, capture_output=True, text=True
            )
            self.assertEqual(out.returncode, 0, out.stderr)
            report = json.loads(out.stdout)
            self.assertEqual(report["count"], 1)
            self.assertEqual(report["flows"][0]["status"], "completed")
            self.assertEqual(report["flows"][0]["steps_done"], 10)
            self.assertEqual(report["flows"][0]["steps_total"], 10)


if __name__ == "__main__":
    unittest.main()
