"""Policy loading and function-target extraction.

`keel.toml` is parsed with stdlib `tomllib` into the plain dict the backend's
`configure` validates (SEMANTIC validation — durations, schedules, rates,
unknown keys — is the backend's job, so front end and backend never diverge).
An absent file yields the embedded Level 0 defaults (DX spec §1). A file that
is present but unparseable or unreadable is a LOUD failure (KEEL-E001), never
a silent fall-back to defaults — a Level 0 surprise is a P0 (DX spec §1).
"""

from __future__ import annotations

import tomllib
from pathlib import Path
from typing import Any, NamedTuple

from ._defaults import level0_defaults
from ._errors import KeelError


class LoadedPolicy(NamedTuple):
    policy: dict[str, Any]
    source: str  # "keel.toml" | "defaults"


def load_policy(cwd: str | Path | None = None) -> LoadedPolicy:
    """Load `<cwd>/keel.toml`, or the Level 0 embedded pack if absent."""
    path = Path(cwd or Path.cwd()) / "keel.toml"
    if not path.exists():
        return LoadedPolicy(level0_defaults(), "defaults")
    try:
        raw = path.read_bytes()
    except OSError as exc:  # present but unreadable → loud, not silent
        raise KeelError(
            "KEEL-E001",
            f"keel.toml is present but could not be read: {exc}. Fix the file's "
            "permissions/path, or remove it to fall back to Level 0 defaults.",
        ) from exc
    try:
        policy = tomllib.loads(raw.decode("utf-8"))
    except (tomllib.TOMLDecodeError, UnicodeDecodeError) as exc:
        raise KeelError("KEEL-E001", f"keel.toml is not valid TOML: {exc}") from exc
    return LoadedPolicy(policy, "keel.toml")


class FunctionTarget(NamedTuple):
    """A wrappable `py:` function target parsed into the module it applies to
    and a glob over that module's function names."""

    key: str  # the policy target key, e.g. "py:pipeline.enrich.*"
    module: str  # the module whose functions it selects, e.g. "pipeline.enrich"
    func_glob: str  # fnmatch pattern over function names, e.g. "*"


def extract_function_targets(policy: dict[str, Any]) -> list[FunctionTarget]:
    """The `py:` function targets declared in policy.

    v0.1 rule (documented): a `py:<module>.<func>` key wraps module-level
    functions of the exactly-named module `<module>` whose name matches
    `<func>` (an fnmatch glob; `*` selects all). The module portion must be
    concrete — mid-path globs like `py:pkg.*.run` are out of v0.1 scope and
    are skipped rather than silently mis-wrapped.
    """
    targets = policy.get("target")
    if not isinstance(targets, dict):
        return []
    out: list[FunctionTarget] = []
    for key in targets:
        if not isinstance(key, str) or not key.startswith("py:"):
            continue
        body = key[3:]
        if "." not in body:
            continue  # need at least module.func
        module, func_glob = body.rsplit(".", 1)
        if not module or "*" in module:
            continue  # mid-path globs unsupported in v0.1
        out.append(FunctionTarget(key=key, module=module, func_glob=func_glob))
    return out


class FlowEntrypoint(NamedTuple):
    """A Tier 2 durable-flow entrypoint from `[flows] entrypoints`, parsed from
    the `py:<module>:<function>` grammar into the module to import and the
    function to run as the flow body. The module portion may be a `*` glob
    over the dotted module path (`py:pipeline.*:main`); `match_flow` resolves
    a glob to a CONCRETE entry (so flow identity never contains a glob), with
    `via` recording the designating pattern."""

    raw: str  # the entrypoint, e.g. "py:pipeline:main" (concrete after a glob match)
    module: str  # the module to import — or a `*` glob before matching
    function: str  # the function to run as the flow, e.g. "main"
    via: str | None = None  # the declared glob that designated this entry, if any


def extract_flow_entrypoints(policy: dict[str, Any]) -> list[FlowEntrypoint]:
    """The `py:` flow entrypoints declared in `[flows] entrypoints`.

    Admitted forms (docs/targeting.md; the frozen `entrypointRef` grammar
    `^(py|ts|rs):\\S+$` already covers all of them — no schema change):

      * `py:<module>:<function>` — concrete designation, as before. The module
        is a concrete importable dotted path; the function is its flow body.
      * `py:<module-glob>:<function>` — the module portion may contain `*`
        (matching any run of characters, dots included, over the dotted module
        path of the script `keel run` executes). The function must stay
        CONCRETE — a `*` in the function position is skipped (we must know
        exactly what to call).
      * `py:<module-glob>` — glob shorthand with no `:function`: the function
        defaults to `main` (the spec's own convention — `keel flows add
        pipeline.ingest:main`). A colon-less entry WITHOUT a glob stays
        skipped, exactly as before.

    Malformed and non-`py:` entries are skipped, not guessed — designating a
    flow is an explicit, load-bearing assertion.
    """
    flows = policy.get("flows")
    if not isinstance(flows, dict):
        return []
    entrypoints = flows.get("entrypoints")
    if not isinstance(entrypoints, list):
        return []
    out: list[FlowEntrypoint] = []
    for raw in entrypoints:
        if not isinstance(raw, str) or not raw.startswith("py:"):
            continue
        body = raw[3:]
        if ":" in body:
            module, function = body.rsplit(":", 1)
            if not module or not function or "*" in function:
                continue  # the flow body must be a concrete, named function
        elif "*" in body:
            module, function = body, "main"  # glob shorthand: py:pipeline.* ≡ py:pipeline.*:main
        else:
            continue  # a concrete module still needs its :function
        out.append(FlowEntrypoint(raw=raw, module=module, function=function))
    return out
