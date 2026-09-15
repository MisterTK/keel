"""Submit-then-poll against a fake Vertex LRO server on 127.0.0.1 through the
REAL httpx transport seam. Prints POLLS=<n> DONE=<bool>. Exits 3 when the poll
loop never observes done (the 2026-09-15 field pathology).

The one in-process shim is the host map: the backend's `resolve_target` only
knows the real provider hostnames, so a loopback fixture server would resolve
to the bare host and never reach the `llm:*` branches this test is about.
Mapping 127.0.0.1 onto `llm:google-genai` per backend instance is the same
seam `tests/test_llm_budget_fallback.py::_LlmSeamBase` uses. Everything else —
activation, the httpx pack, the dev cache, the LRO-shape exemption — is the
real machinery in a real child process.
"""

import os
import sys
import time

import httpx

from keel import _runtime

BASE = os.environ["LRO_BASE"]  # http://127.0.0.1:<port>/v1/…/models/veo-3.1

backend = _runtime.get_backend()
if backend is not None:
    _orig = backend.resolve_target

    def _mapped(method, host, *, scheme=None, port=None, path=None):
        if host == "127.0.0.1":
            return "llm:google-genai"
        return _orig(method, host, scheme=scheme, port=port, path=path)

    backend.resolve_target = _mapped  # type: ignore[method-assign]

with httpx.Client(timeout=10.0) as c:
    op = c.post(f"{BASE}:predictLongRunning", json={"instances": [{"prompt": "a cat"}]}).json()
    polls = 0
    done = False
    while polls < 10:
        polls += 1
        body = c.post(f"{BASE}:fetchPredictOperation", json={"operationName": op["name"]}).json()
        if body.get("done") is True:
            done = True
            break
        time.sleep(0.05)
print(f"POLLS={polls} DONE={done}")
sys.exit(0 if done else 3)
