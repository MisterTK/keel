# keel-adk-template

A Google [`agents-cli`](https://github.com/google/adk-python) remote template:
the stock `adk` base template (a simple ReAct agent, A2A-enabled) plus Keel
resilience wired in — no code changes.

## What this adds over the base `adk` template

- **`keelrun` dependency** — added to `pyproject.toml` alongside the base
  template's own `google-adk[gcp]`, `a2a-sdk[http-server]`, and `aiohttp`
  pins (agents-cli's remote-template deep-merge *replaces* the dependency
  list wholesale rather than appending to it, so the base's three deps are
  repeated here — see `agents-cli-manifest.yaml`).
- **`app/keel.toml`** — a starter Keel policy, placed *inside* the agent
  directory on purpose. The generated `Dockerfile` only `COPY`s
  `pyproject.toml`, `README.md`, `uv.lock*`, and the agent directory itself
  into the image; a `keel.toml` at the project root would silently never
  reach the container (`keel doctor` warns about exactly this trap).
- **`KEEL_ENABLE=1`** — appended to `.env.example`. `keelrun`'s import-hook
  shim only activates when this is set, so local dev and the container both
  need it in their environment.

Everything else — `agent.py`, the FastAPI app, tests, CI/CD, deployment
targets — comes from the base `adk` template untouched.

## Use it

```
agents-cli create my-agent -a MisterTK/keel/packaging/agents-cli-template
```

This fetches the template from this repo's subdirectory and layers it over
the rendered `adk` base template. Then, from the generated project:

```
cd my-agent
keel doctor      # verifies keel.toml placement, dependency wiring, .env
agents-cli install && agents-cli playground
```

## Note for maintainers

This directory is written to double as the source for a standalone
`keel-adk-template` repo (a plain `git subtree`/`git filter-repo` push) if a
top-level, non-subdirectory template ever becomes worth publishing
separately — nothing here assumes it lives inside the `keel` monorepo.
