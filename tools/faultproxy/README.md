# faultproxy

A scriptable, deterministic fault-injecting HTTP server/proxy in a single
stdlib-only Python file. It gives the demos (and, optionally, the adapter tests)
a reusable fault source with **no build step and no dependencies** — every Keel
team already has `python3`, which is why this is Python and not a Rust binary.

## Why deterministic

Each request *path* is served from its own ordered sequence of directives: the
Nth request to a path gets the Nth directive. No randomness, no timers you can't
control — so a demo behaves identically every run and a test can assert exact
call counts. (This is the per-path generalization of the test suite's
single-queue `python/keel/tests/faultserver.py`.)

## Run

```
python3 tools/faultproxy/faultproxy.py --scenario scenario.json --port 0 --port-file /tmp/fp.port
# stdout first line: PORT=<n>   (also written to --port-file)
```

## Scenario file (JSON)

```json
{
  "upstream": "http://127.0.0.1:9000",
  "default": { "status": 200, "body": "ok" },
  "paths": {
    "/flaky": [ {"status": 503}, {"status": 200, "body": "late"} ],
    "/v1/chat/completions": [
      {"status": 429, "headers": {"Retry-After": "0"}, "repeat": 2},
      {"status": 200, "body": "{\"reply\":\"hi\"}", "headers": {"content-type": "application/json"}}
    ],
    "*": [ {"forward": true} ]
  }
}
```

Directive fields (all optional): `status` (default 200), `body` (str),
`headers` (obj), `delay_ms`, `reset` (drop the connection), `repeat` (serve this
directive N times before advancing), `forward` (reverse-proxy to `upstream`).

When a path's sequence is spent: `default` → forward to `upstream` (if set) →
plain `200`.

## Control endpoints

- `GET /__faultproxy__/log` → JSON of `{method, path, status}` served so far.
- `POST /__faultproxy__/reset` → reset per-path cursors and the log.

## Embed in a test

`Scenario` and `FaultProxy` are importable; `Scenario.next_directive(path)` is
the pure sequencing core (unit-tested in `test_faultproxy.py`).

```python
from faultproxy import FaultProxy, Scenario
with FaultProxy(Scenario({"paths": {"/x": [{"status": 503}, {"status": 200}]}})) as proxy:
    ...  # hit proxy.url("/x")
```

## Test

```
python3 tools/faultproxy/test_faultproxy.py
```
