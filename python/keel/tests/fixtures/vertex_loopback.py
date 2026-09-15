"""Talk to a loopback fixture server while Keel sees a REAL Vertex URL.

The slice-1 acceptance fixtures need two things at once: the request Keel
judges must carry a genuine provider hostname (so the core's own host map
resolves ``llm:google-genai`` and the llm-POST branches under test are the
ones that actually run), and the socket must reach a fixture server on
127.0.0.1.

`LoopbackTransport` gets both because of where Keel's httpx seam sits:
`adapters/httpx_pack.py` patches `httpx.Client.__init__` to wrap any
CUSTOM transport instance's `handle_request`, so Keel intercepts at the
outside of this class — seeing the unmodified Vertex URL — and this class
then dials the fixture server itself.

It dials with `http.client` rather than delegating to a nested
`httpx.HTTPTransport`, because the pack also patches
`httpx.HTTPTransport.handle_request` at CLASS level: a nested one would make
Keel intercept every call twice and double-count `calls`.

An earlier version of these fixtures instead monkeypatched
`backend.resolve_target` to map 127.0.0.1 onto `llm:google-genai` (the seam
`tests/test_llm_budget_fallback.py::_LlmSeamBase` uses). That works on the
pure-Python stub but raises `AttributeError: 'keel_core.KeelCore' object
attribute 'resolve_target' is read-only` against the native PyO3 core, so the
acceptance gate silently covered only the stub. Resolving a real hostname
needs no shim and holds on both backends.
"""

from __future__ import annotations

import http.client
import os

import httpx

#: A real Vertex endpoint — the host map must recognize it for these fixtures
#: to exercise anything. Nothing ever connects here: `LoopbackTransport`
#: rewrites the destination, not the URL.
VERTEX_BASE = (
    "https://us-central1-aiplatform.googleapis.com"
    "/v1/projects/p/locations/us-central1/publishers/google/models/veo-3.1"
)

#: "127.0.0.1:<port>" — where the fixture server is actually listening.
LOCAL_AUTHORITY = os.environ["LRO_LOCAL"]


class LoopbackTransport(httpx.BaseTransport):
    """Send the request Keel just judged to the fixture server instead."""

    def handle_request(self, request: httpx.Request) -> httpx.Response:
        body = request.read()
        conn = http.client.HTTPConnection(LOCAL_AUTHORITY, timeout=10)
        try:
            conn.request(
                request.method,
                request.url.raw_path.decode("ascii"),
                body=body,
                headers={
                    "content-type": "application/json",
                    "content-length": str(len(body)),
                },
            )
            upstream = conn.getresponse()
            data = upstream.read()
            # Let httpx recompute the framing headers for the body we hand it;
            # copying the originals would contradict it.
            headers = [
                (k, v)
                for k, v in upstream.getheaders()
                if k.lower() not in ("content-length", "transfer-encoding")
            ]
            return httpx.Response(
                upstream.status, headers=headers, content=data, request=request
            )
        finally:
            conn.close()


def loopback_client(**kwargs: object) -> httpx.Client:
    """An `httpx.Client` whose transport Keel will wrap (see module docstring)."""
    return httpx.Client(transport=LoopbackTransport(), **kwargs)  # type: ignore[arg-type]
