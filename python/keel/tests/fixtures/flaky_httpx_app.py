"""Sprint Team-B acceptance fixture: a bare httpx script against a flaky server.

The server returns 503 on the first hit, then 200. Run plainly, the first 503
makes ``raise_for_status()`` raise and the process exits non-zero. Run under
``python -m keel run`` (Level 0 defaults retry 5xx), the 503 is retried at the
transport seam and the script sees the 200 — same code, now resilient.
"""

import os
import sys

import httpx

url = os.environ["KEEL_DEMO_URL"]
resp = httpx.get(url, timeout=5.0)
resp.raise_for_status()
sys.stdout.write("flaky ok\n")
