#!/usr/bin/env python3
"""Tests for faultproxy: the pure per-path sequencing (no network) and one live
end-to-end hit over a loopback socket. Stdlib only.

Run: `python3 tools/faultproxy/test_faultproxy.py`
"""

from __future__ import annotations

import json
import sys
import unittest
import urllib.request
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from faultproxy import FaultProxy, Scenario  # noqa: E402


class SequencingTest(unittest.TestCase):
    def test_per_path_sequences_are_independent_and_ordered(self) -> None:
        s = Scenario(
            {
                "paths": {
                    "/a": [{"status": 503}, {"status": 200, "body": "a-ok"}],
                    "/b": [{"status": 500}],
                }
            }
        )
        # /a advances independently of /b.
        self.assertEqual(s.next_directive("/a")["status"], 503)
        self.assertEqual(s.next_directive("/b")["status"], 500)
        self.assertEqual(s.next_directive("/a")["status"], 200)
        self.assertEqual(s.next_directive("/a")["body"], "faultproxy: no directive")  # /a spent

    def test_repeat_serves_directive_n_times_before_advancing(self) -> None:
        s = Scenario(
            {"paths": {"/x": [{"status": 429, "repeat": 3}, {"status": 200, "body": "done"}]}}
        )
        self.assertEqual([s.next_directive("/x")["status"] for _ in range(3)], [429, 429, 429])
        self.assertEqual(s.next_directive("/x")["status"], 200)

    def test_spent_sequence_uses_default_when_present(self) -> None:
        s = Scenario({"default": {"status": 204}, "paths": {"/y": [{"status": 500}]}})
        self.assertEqual(s.next_directive("/y")["status"], 500)
        self.assertEqual(s.next_directive("/y")["status"], 204)
        self.assertEqual(s.next_directive("/y")["status"], 204)  # default is stable, not consumed

    def test_wildcard_path_is_the_fallback(self) -> None:
        s = Scenario({"paths": {"*": [{"status": 418}]}})
        self.assertEqual(s.next_directive("/anything")["status"], 418)
        # Wildcard sequence is itself ordered + then spent → plain 200.
        self.assertEqual(s.next_directive("/anything")["status"], 200)

    def test_reset_rewinds_all_cursors(self) -> None:
        s = Scenario({"paths": {"/z": [{"status": 503}, {"status": 200}]}})
        self.assertEqual(s.next_directive("/z")["status"], 503)
        s.reset()
        self.assertEqual(s.next_directive("/z")["status"], 503)

    def test_malformed_scenario_is_rejected(self) -> None:
        with self.assertRaises(ValueError):
            Scenario({"paths": {"/p": {"status": 200}}})  # not a list


class LiveHitTest(unittest.TestCase):
    def test_503_then_200_over_a_real_socket(self) -> None:
        scenario = Scenario({"paths": {"/flaky": [{"status": 503}, {"status": 200, "body": "ok"}]}})
        with FaultProxy(scenario) as proxy:
            with self.assertRaises(urllib.error.HTTPError) as ctx:
                urllib.request.urlopen(proxy.url("/flaky"), timeout=5)
            self.assertEqual(ctx.exception.code, 503)
            ctx.exception.close()  # release the socket so no ResourceWarning
            with urllib.request.urlopen(proxy.url("/flaky"), timeout=5) as resp:
                self.assertEqual(resp.status, 200)
                self.assertEqual(resp.read(), b"ok")
            # The control log records exactly what was served.
            with urllib.request.urlopen(proxy.url("/__faultproxy__/log"), timeout=5) as resp:
                served = json.loads(resp.read())
            self.assertEqual([e["status"] for e in served], [503, 200])


if __name__ == "__main__":
    unittest.main()
