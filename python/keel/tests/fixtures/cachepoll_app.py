"""#78 acceptance fixture: POST the SAME `:generateContent` body six times,
sleeping ~4.5s between each call — a status-poll cadence, not a test-suite
burst — against the real Vertex hostname `vertex_loopback` uses. Unlike the
LRO submit/poll shapes (`:predictLongRunning`/`:fetchPredictOperation`,
exempted from deriving a cache key by #83), `:generateContent` is a shape the
default dev cache still hashes, so five of these six calls genuinely replay
from cache — reproducing the pathology Keel's runtime cache-poll detector
(WS9, #78) is meant to name, rather than proving nothing.
"""

import time

from vertex_loopback import VERTEX_BASE, loopback_client

BODY = {"contents": [{"role": "user", "parts": [{"text": "status?"}]}]}

with loopback_client(timeout=10.0) as c:
    for i in range(6):
        c.post(f"{VERTEX_BASE}:generateContent", json=BODY)
        if i < 5:
            time.sleep(4.5)
