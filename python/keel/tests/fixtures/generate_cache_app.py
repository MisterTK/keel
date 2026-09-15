"""The acceptance suite's control: two identical prompt-shaped POSTs
(`:generateContent`) through the same real httpx seam and the same real Vertex
hostname the LRO fixture uses.

The dev cache is expected to replay the second one. That is what proves
configuration (c)'s zero cache hits come from the LRO-shape exemption and not
from the dev cache being inert — or from the host map failing to recognize the
endpoint at all, which would leave every call on a non-`llm:` target and make
(c) vacuously green.
"""

from vertex_loopback import VERTEX_BASE, loopback_client

with loopback_client(timeout=10.0) as c:
    for _ in range(2):
        c.post(f"{VERTEX_BASE}:generateContent", json={"contents": []})
