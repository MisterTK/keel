"""`keel record run`, end to end in a child process (docs/recording-format.md):
`KEEL_RECORD` actually wires the tee into the live runtime for a real `keel
run` of a `py:` function target, and the file it writes matches the format.

This is the regression test for a real bug caught during manual verification:
`install_keel`'s returned `state["backend"]` is NOT what `py:`/HTTP wrappers
read (they call `keel._runtime.get_backend()` dynamically) — wrapping the
backend without also calling `_runtime.set_runtime` silently recorded nothing.
"""

from __future__ import annotations

import json
import subprocess
import sys
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory

from . import FIXTURES, child_env

ENRICH = str(FIXTURES / "enrich_app.py")


class RecordCaptureTest(unittest.TestCase):
    def test_keel_record_run_via_keel_record_writes_meta_and_call_lines(self) -> None:
        toml = '[target."py:sample_targets.enrich_*"]\n'
        with TemporaryDirectory() as d:
            (Path(d) / "keel.toml").write_text(toml, encoding="utf-8")
            record_path = Path(d) / ".keel" / "recordings" / "r.ndjson"
            proc = subprocess.run(
                [sys.executable, "-m", "keel", "run", ENRICH],
                env=child_env(KEEL_RECORD=str(record_path)),
                cwd=d,
                capture_output=True,
            )
            self.assertEqual(proc.returncode, 0, proc.stderr.decode())
            self.assertEqual(proc.stdout, b"enriched 42\n", "recording changes no program output")
            self.assertIn("recording to", proc.stderr.decode())

            self.assertTrue(record_path.exists(), "the front end, not the CLI, writes the file")
            lines = [json.loads(l) for l in record_path.read_text(encoding="utf-8").splitlines() if l.strip()]
        self.assertGreaterEqual(len(lines), 2, "a meta header plus at least one call line")
        meta = lines[0]
        self.assertEqual(meta["type"], "meta")
        self.assertEqual(meta["language"], "python")
        self.assertEqual(meta["v"], 1)

        calls = [l for l in lines[1:] if l["type"] == "call"]
        self.assertEqual(len(calls), 1, "enrich_app.py makes exactly one wrapped call")
        call = calls[0]
        self.assertEqual(call["target"], "py:sample_targets.enrich_*")
        self.assertEqual(call["outcome"]["result"], "ok")
        self.assertEqual(call["outcome"]["payload"], 42)
        self.assertTrue(call["body_captured"])

    def test_without_keel_record_no_recording_directory_is_created(self) -> None:
        toml = '[target."py:sample_targets.enrich_*"]\n'
        with TemporaryDirectory() as d:
            (Path(d) / "keel.toml").write_text(toml, encoding="utf-8")
            proc = subprocess.run(
                [sys.executable, "-m", "keel", "run", ENRICH],
                env=child_env(),
                cwd=d,
                capture_output=True,
            )
            self.assertEqual(proc.returncode, 0, proc.stderr.decode())
            self.assertNotIn("recording to", proc.stderr.decode())
            self.assertFalse((Path(d) / ".keel" / "recordings").exists())


if __name__ == "__main__":
    unittest.main()
