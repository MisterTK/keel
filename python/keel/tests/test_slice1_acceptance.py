"""Slice-1 acceptance (deployment-honesty program, spec §4): the ai-marketing-hub
2026-09-15 incident, reproduced end to end through real child processes and the
real httpx seam, in the three configurations the spec names.

The incident: `keel.toml` was never COPY'd into the Cloud Run image, so
`KEEL_ENABLE=1 KEEL_CWD=/code` activated Keel on production defaults; the
default `[defaults.llm]` dev cache hashed a POST-shaped Vertex LRO poll
(`:fetchPredictOperation`, a byte-identical body every 10s) and replayed the
first "still running" answer until the app's own 900s deadline.

The fake Vertex server is a local `http.server` thread rather than
`tests/faultserver.py`'s `FaultServer`: that helper pops directives from a
single ordered queue shared across paths and cannot key a response body on the
POST path, which is exactly what a submit-then-poll transcript (and the poll
counter these tests assert on) needs.
"""

from __future__ import annotations

import json
import subprocess
import sys
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from tempfile import TemporaryDirectory

from . import FIXTURES, child_env

APP = str(FIXTURES / "lro_poll_app.py")
GENERATE_APP = str(FIXTURES / "generate_cache_app.py")
CACHEPOLL_APP = str(FIXTURES / "cachepoll_app.py")


class _FakeVertex(BaseHTTPRequestHandler):
    """POST :predictLongRunning → an operation; POST :fetchPredictOperation →
    pending twice, then done. Byte-identical poll bodies, like Vertex."""

    polls = 0

    def do_POST(self) -> None:  # noqa: N802
        length = int(self.headers.get("content-length", "0"))
        self.rfile.read(length)
        if self.path.endswith(":predictLongRunning"):
            body = {"name": "projects/p/locations/us-central1/operations/op1"}
        else:
            type(self).polls += 1
            body = {
                "name": "projects/p/locations/us-central1/operations/op1",
                "done": type(self).polls >= 3,
            }
        data = json.dumps(body).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, *_: object) -> None:
        pass


class Slice1AcceptanceTest(unittest.TestCase):
    def setUp(self) -> None:
        _FakeVertex.polls = 0
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), _FakeVertex)
        self.server.daemon_threads = True
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        # Only the authority travels to the child: the fixtures address a real
        # Vertex URL so the core's own host map resolves `llm:google-genai`,
        # and their transport redirects the socket here (see
        # `fixtures/vertex_loopback.py`).
        self.local_authority = f"127.0.0.1:{self.server.server_address[1]}"
        self._tmp = TemporaryDirectory()
        self.empty_root = Path(self._tmp.name)  # no keel.toml here — the image without COPY

    def tearDown(self) -> None:
        self.server.shutdown()
        self.server.server_close()
        self._tmp.cleanup()

    def _run(self, **env: str) -> subprocess.CompletedProcess[bytes]:
        return subprocess.run(
            [
                sys.executable,
                "-c",
                "import keel._auto; import runpy; runpy.run_path(%r, run_name='__main__')" % APP,
            ],
            env=child_env(
                **{
                    "KEEL_ENABLE": "1",
                    "KEEL_CWD": str(self.empty_root),
                    "LRO_LOCAL": self.local_authority,
                    "KEEL_LOG_FORMAT": "json",  # overridable by a caller (b2)
                    **env,
                }
            ),
            cwd=str(self.empty_root),
            capture_output=True,
            timeout=60,
        )

    def _objs(self, proc: subprocess.CompletedProcess[bytes]) -> list[dict]:
        return [
            json.loads(line)
            for line in proc.stderr.decode().splitlines()
            if line.strip().startswith("{")
        ]

    def test_a_strict_refusal_runs_keel_free_and_the_render_completes(self) -> None:
        proc = self._run()
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn(b"DONE=True", proc.stdout)
        objs = self._objs(proc)
        self.assertEqual([o["keel"] for o in objs], ["error"])
        self.assertEqual(objs[0]["code"], "policy-missing-at-keel-cwd")
        self.assertEqual(_FakeVertex.polls, 3)

    def test_b_optional_in_a_serverless_env_has_the_dev_cache_off(self) -> None:
        proc = self._run(KEEL_POLICY="optional", K_SERVICE="render")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn(b"POLLS=3 DONE=True", proc.stdout)
        objs = self._objs(proc)
        self.assertEqual([o["keel"] for o in objs], ["activation", "summary"])
        self.assertEqual(objs[0]["policy_source"], "defaults")
        # The demotion is a NAMED FIELD, not just banner prose — a log pipeline
        # can only index what the object names. Its text twin is asserted below.
        self.assertEqual(objs[0]["dev_cache_off"], "K_SERVICE")
        self.assertEqual(objs[1]["cache_hits"], 0)

    def test_b2_the_text_banner_says_the_same_thing_as_the_json_field(self) -> None:
        proc = self._run(KEEL_POLICY="optional", K_SERVICE="render", KEEL_LOG_FORMAT="text")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn(b"(dev cache off: K_SERVICE detected)", proc.stderr)

    def test_b3_an_explicit_keel_env_prod_is_named_as_the_reason_too(self) -> None:
        # The other way a deployment turns the dev cache off (and the one the
        # docs recommend when no marker is present). The banner's parenthetical
        # is marker-only prose, so the JSON field is the ONLY place this shows
        # up — reporting null here would be a lie about a production process.
        proc = self._run(KEEL_POLICY="optional", KEEL_ENV="prod")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn(b"POLLS=3 DONE=True", proc.stdout)
        objs = self._objs(proc)
        self.assertEqual(objs[0]["dev_cache_off"], "KEEL_ENV")
        self.assertEqual(objs[-1]["cache_hits"], 0)

    def test_c_optional_alone_the_field_configuration_no_longer_replays_polls(self) -> None:
        # No serverless marker, dev cache ON by default: only the LRO-shape
        # exemption (#83) stands between this run and the 15-minute hang.
        proc = self._run(KEEL_POLICY="optional")
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn(b"POLLS=3 DONE=True", proc.stdout)
        objs = self._objs(proc)
        # Self-evidencing: the dev cache was genuinely ON in THIS process (no
        # marker, no KEEL_ENV), so the zero below is the LRO exemption holding
        # — not a cache that happened to be off. (d) is the second, independent
        # witness that the cache still replays when the shape allows it.
        self.assertIsNone(objs[0]["dev_cache_off"], "the dev cache must be ON for (c) to mean anything")
        self.assertEqual(objs[-1]["keel"], "summary")
        self.assertEqual(objs[-1]["cache_hits"], 0, "a cache hit here is the field outage")
        self.assertEqual(objs[-1]["calls"], 4)  # 1 submit + 3 polls, all intercepted
        self.assertEqual(_FakeVertex.polls, 3)

    def test_d_a_generate_call_still_replays_from_the_dev_cache(self) -> None:
        # Control: the dev cache itself still works for prompt-shaped POSTs, so
        # (c) passes because of the LRO exemption, not because caching is dead —
        # and not because the host map failed to recognize the endpoint, which
        # would leave every call on a non-`llm:` target and make (c) vacuous.
        proc = subprocess.run(
            [
                sys.executable,
                "-c",
                "import keel._auto; import runpy; runpy.run_path(%r, run_name='__main__')"
                % GENERATE_APP,
            ],
            env=child_env(
                KEEL_ENABLE="1",
                KEEL_CWD=str(self.empty_root),
                KEEL_POLICY="optional",
                LRO_LOCAL=self.local_authority,
                KEEL_LOG_FORMAT="json",
            ),
            cwd=str(self.empty_root),
            capture_output=True,
            timeout=60,
        )
        self.assertEqual(proc.returncode, 0, proc.stderr)
        objs = self._objs(proc)
        self.assertEqual(objs[-1]["cache_hits"], 1, proc.stderr)

    def test_f_a_status_poll_shaped_cache_replay_is_named_at_runtime(self) -> None:
        # #78: six identical `:generateContent` POSTs, ~4.5s apart (a status-
        # poll cadence, not a burst) — `:generateContent` is a shape the dev
        # cache still hashes (unlike the LRO submit/poll shapes #83 exempts),
        # so this genuinely reproduces five consecutive cache hits rather than
        # proving nothing. KEEL_CACHEPOLL_MIN_SPAN_S is an UNSTABLE, test-only
        # knob (read once at detector construction) that shrinks the ~18s
        # span this cadence produces below the threshold without changing the
        # production default (20s), so the whole run finishes in ~25s.
        proc = subprocess.run(
            [
                sys.executable,
                "-c",
                "import keel._auto; import runpy; runpy.run_path(%r, run_name='__main__')"
                % CACHEPOLL_APP,
            ],
            env=child_env(
                KEEL_ENABLE="1",
                KEEL_CWD=str(self.empty_root),
                KEEL_POLICY="optional",
                LRO_LOCAL=self.local_authority,
                KEEL_LOG_FORMAT="json",
                KEEL_CACHEPOLL_MIN_SPAN_S="15",
            ),
            cwd=str(self.empty_root),
            capture_output=True,
            timeout=60,
        )
        self.assertEqual(proc.returncode, 0, proc.stderr)
        objs = self._objs(proc)
        suspects = [o for o in objs if o.get("code") == "cache-poll-suspect"]
        self.assertEqual(len(suspects), 1, proc.stderr)
        self.assertEqual(suspects[0]["hits"], 5)
        self.assertEqual(objs[-1]["keel"], "summary")
        self.assertEqual(objs[-1]["cache_poll_suspects"], 1)
