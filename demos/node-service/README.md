# node-service

The Level 0 story for Node: a bare `fetch` call dies on a transient failure,
made resilient by `keel run` with **zero code changes** (the Node loader
intercepts `fetch`/`undici`).

```
./run.sh          # needs node >= 22.5
```

- **faultproxy** serves `/svc` as `500` then `200`.
- **Bare** `node app.mjs`: `!resp.ok` throws on the 500 → non-zero exit.
- **`keel run app.mjs`** (from source: `node .../bin/keel-node-run.mjs app.mjs`):
  Level 0 defaults retry the 5xx → the fetch returns the `200` and it prints
  `service ok`.

Tier 1 retry works on the pure-Node core; no native addon required.

Smoke-tested by `node/keel/test/demo.e2e.test.mjs`.
