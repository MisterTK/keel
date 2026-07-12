"""A bare, unremarkable httpx script. It has no idea Keel exists.

Run plainly it dies on the first transient 503. Run under `keel run` it
survives — same code, now production-grade (Level 0 defaults retry 5xx).
"""

import os
import sys

import httpx

url = os.environ["KEEL_DEMO_URL"]
resp = httpx.get(url, timeout=5.0)
resp.raise_for_status()
sys.stdout.write("flaky ok\n")
