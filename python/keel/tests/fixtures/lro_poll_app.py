"""Submit-then-poll against a fake Vertex LRO server through the REAL httpx
transport seam. Prints POLLS=<n> DONE=<bool>. Exits 3 when the poll loop never
observes done (the 2026-09-15 field pathology).

No shim: the URL carries a real Vertex hostname, so the core's own host map
resolves `llm:google-genai` and the llm-POST cache branches under test are the
ones that really run — on the native core as well as the stub. The socket
reaches the fixture server via `vertex_loopback.LoopbackTransport`; see that
module for why Keel still sees the Vertex URL.
"""

import sys
import time

from vertex_loopback import VERTEX_BASE, loopback_client

with loopback_client(timeout=10.0) as c:
    op = c.post(
        f"{VERTEX_BASE}:predictLongRunning", json={"instances": [{"prompt": "a cat"}]}
    ).json()
    polls = 0
    done = False
    while polls < 10:
        polls += 1
        body = c.post(
            f"{VERTEX_BASE}:fetchPredictOperation", json={"operationName": op["name"]}
        ).json()
        if body.get("done") is True:
            done = True
            break
        time.sleep(0.05)
print(f"POLLS={polls} DONE={done}")
sys.exit(0 if done else 3)
