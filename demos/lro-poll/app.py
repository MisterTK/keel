"""Wait for a long-running operation to finish, then use its result.

This is the whole app, and it is the SAME code in both acts. It makes ONE
`httpx.get` of a `google.longrunning.Operation` and then reads the result off
the body — because the `[target."127.0.0.1"] poll` block in `keel.toml` is
supposed to have turned that one GET into poll-until-terminal. Code written
against a poll policy is allowed to assume the body it gets back is terminal;
that assumption is exactly what the policy is for.

Act 1 runs it under a pre-CCR-11 poll block and the assumption is false: the
poll fails open on the missing `done` and hands back the still-running body,
so `op["response"]` raises KeyError. Act 2 runs the same code under the same
block plus `absent = "pending"` and it gets `gs://out/video.mp4`.

`resp.keel_outcome["attempts"]` is Keel's own count of upstream HTTP attempts
behind this one call — 1 when the poll did not poll, 4 when it did. It is
printed because Keel's exit summary does not break poll attempts out (it
counts one `call` either way); see this demo's README.
"""

from __future__ import annotations

import json
import os
import sys

import httpx

url = os.environ["KEEL_DEMO_URL"]  # faultproxy's .../operations/123
resp = httpx.get(url, timeout=10.0)
resp.raise_for_status()
op = resp.json()

attempts = getattr(resp, "keel_outcome", {}).get("attempts")
sys.stdout.write(f"operation body: {json.dumps(op, separators=(',', ':'), sort_keys=True)}\n")
sys.stdout.write(f"done={op.get('done')} upstream_attempts={attempts}\n")

# The line that trusts the poll. Under a poll block that never polled, this is
# where an app that "adopted" Keel finds out it did not.
uri = op["response"]["generatedSample"]["uri"]
sys.stdout.write(f"video: {uri}\n")
