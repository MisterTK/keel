"""A tiny "agent" step: fetch a completion from a (fake, flaky) LLM endpoint.

The completion call is modeled as an ordinary intercepted HTTP GET, so Keel's
resilience applies with zero agent-code changes:

  * a 429 rate-limit storm is ridden out (retry with backoff), and
  * off-`KEEL_ENV=prod` the successful response is dev-cached — so a SECOND run
    with the same prompt replays from the journal and makes ~0 API calls (the
    "10x faster iteration, near-zero API spend" dev-loop win).

Cross-run replay needs the native core + a journal; without it the second run
simply calls again (still correct, just not free).
"""

import os
import sys

import httpx

url = os.environ["KEEL_DEMO_URL"]  # faultproxy's /v1/complete
resp = httpx.get(url, timeout=10.0)
resp.raise_for_status()
reply = resp.json().get("reply")
cached = bool(getattr(resp, "keel_outcome", {}).get("from_cache"))
sys.stdout.write(f"reply={reply} from_cache={cached}\n")
