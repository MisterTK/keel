# flaky-python

The Level 0 hero (dx-spec §1): a bare httpx script that dies on a transient
failure, made production-grade by `keel run` with **zero code changes**.

```
./run.sh
```

- **faultproxy** serves `/flaky` as `503` then `200` (deterministic, no network).
- **Bare** `python app.py`: the 503 makes `raise_for_status()` throw → non-zero exit.
- **`keel run app.py`**: Level 0 defaults retry the 5xx at the transport seam →
  the script sees the `200` and prints `flaky ok`.

No `keel.toml` needed — smart defaults ship in the binary. `keel init` would
write a policy you can customize.

Smoke-tested by `python/keel/tests/test_demos.py::FlakyDemoRunTest`.
