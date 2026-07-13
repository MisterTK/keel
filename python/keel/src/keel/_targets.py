"""Outbound target selection — exact host > URL pattern > class default.

The frozen target grammar (contracts/policy.schema.json `$defs.targetKey`)
admits host/URL *patterns* for outbound keys — an optional METHOD prefix, a
host that may contain ``*``, an optional ``:port``, and an optional ``/path``
glob — e.g. ``*.internal.corp``, ``GET api.catalog.internal/*``,
``api.stripe.com/v1/*``. Selecting which policy key applies to a request is a
FRONT-END judgment (like the LLM host map and idempotency): the front end
picks one key per request and passes it to the core verbatim, so the core's
resolution stays what it has always been — exact key, then class defaults —
and the engine/stub semantics do not change. The full normative rules live in
``docs/targeting.md``; this module and ``node/keel/src/judge.mjs`` implement
them and MUST stay in parity.

Selection precedence, per request:

  1. exact — a bare-host key equal to the request's host (no method prefix,
     no port, no path, no ``*``). Identical to the pre-pattern behavior.
  2. pattern — every other outbound-shaped key that matches the request's
     method + host + effective port + path. Most specific wins: fewest ``*``,
     then most literal characters, then method-prefixed over unprefixed, then
     lexicographically smallest (pure determinism).
  3. class default — no key matches: the target stays the bare host and the
     core falls through to ``[defaults.outbound]`` as before.

A matched pattern KEY becomes the call's target, so all requests it matches
share that key's breaker, rate limiter, cache namespace, and status line —
one policy target is one dependency. Cache entries still key on the full URL
through ``args_hash``, so pooling never aliases distinct responses.

Matching details (cross-language parity contract):

  * ``*`` matches any run of characters, including ``.`` in hosts and ``/``
    in paths; it is the only metacharacter. Patterns are anchored end-to-end.
  * host comparison is case-insensitive; paths are case-sensitive.
  * a ``:port`` in a key must equal the request's EFFECTIVE port (explicit,
    else 80 for http / 443 for https). A key without a port matches any port.
  * a ``/path`` in a key must match the request's full path (an empty path
    normalizes to ``/``). A key without a path matches any path.
  * a METHOD prefix must equal the request method; no prefix matches all.
"""

from __future__ import annotations

import re
from typing import Any, NamedTuple

#: Methods the frozen targetKey grammar admits as a key prefix.
_METHODS = ("GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS")

#: Non-outbound target classes (function + semantic targets) — never host keys.
_CLASS_PREFIXES = ("py:", "ts:", "rs:", "llm:", "tool:", "mcp:")

#: Default ports per scheme, for `:port`-qualified keys (parity with Node).
_SCHEME_PORTS = {"http": 80, "https": 443}


class OutboundPattern(NamedTuple):
    """One compiled pattern-tier key from ``[target]``."""

    key: str  # the policy key, verbatim (what the core will be handed)
    method: str | None  # required method, or None for any
    host_glob: re.Pattern[str]  # anchored, case-insensitive host matcher
    port: int | None  # required effective port, or None for any
    path_glob: re.Pattern[str] | None  # anchored path matcher, or None for any
    wildcards: int  # specificity: `*` count (fewer wins)
    literal: int  # specificity: non-`*` character count (more wins)


class CompiledTargets(NamedTuple):
    """The outbound view of one effective policy's ``[target]`` table."""

    exact: frozenset[str]  # bare-host keys (tier 1)
    patterns: tuple[OutboundPattern, ...]  # tier 2, pre-sorted most-specific-first


def _glob_regex(glob: str) -> re.Pattern[str]:
    """`*`-only glob → anchored regex. `*` crosses `.` and `/`; everything
    else is literal (never fnmatch: `?`/`[` must stay literal for parity)."""
    return re.compile("^" + ".*".join(re.escape(p) for p in glob.split("*")) + "$")


class _ParsedKey(NamedTuple):
    method: str | None
    host: str
    port: int | None
    path: str | None


def _parse_outbound_key(key: str) -> _ParsedKey | None:
    """Split an outbound key into (method, host, port, path) per the frozen
    grammar, or None when the key is not outbound-shaped (defensive — the
    backend schema-validates keys before we ever compile them)."""
    method = None
    rest = key
    for m in _METHODS:
        if rest.startswith(m + " "):
            method, rest = m, rest[len(m) + 1 :]
            break
    slash = rest.find("/")
    path: str | None = None
    if slash >= 0:
        rest, path = rest[:slash], rest[slash:]
    host, port = rest, None
    head, sep, tail = rest.rpartition(":")
    if sep and tail.isascii() and tail.isdigit():
        host, port = head, int(tail)
    if not host:
        return None
    return _ParsedKey(method, host, port, path)


def compile_outbound_targets(policy: Any) -> CompiledTargets:
    """The outbound matchers of an effective policy's ``[target]`` table."""
    exact: set[str] = set()
    patterns: list[OutboundPattern] = []
    targets = policy.get("target") if isinstance(policy, dict) else None
    if isinstance(targets, dict):
        for key in targets:
            if not isinstance(key, str) or key.startswith(_CLASS_PREFIXES):
                continue
            parsed = _parse_outbound_key(key)
            if parsed is None:
                continue
            if (
                parsed.method is None
                and parsed.port is None
                and parsed.path is None
                and "*" not in parsed.host
            ):
                exact.add(key)
                continue
            wildcards = key.count("*")
            patterns.append(
                OutboundPattern(
                    key=key,
                    method=parsed.method,
                    host_glob=_glob_regex(parsed.host.lower()),
                    port=parsed.port,
                    path_glob=_glob_regex(parsed.path) if parsed.path is not None else None,
                    wildcards=wildcards,
                    literal=len(key) - wildcards,
                )
            )
    # Most specific first; the lexicographic tail makes selection total, so two
    # runs (and two languages) always pick the same key.
    patterns.sort(key=lambda p: (p.wildcards, -p.literal, 0 if p.method else 1, p.key))
    return CompiledTargets(exact=frozenset(exact), patterns=tuple(patterns))


def _matches(
    p: OutboundPattern,
    method: str,
    host: str,
    effective_port: int | None,
    path: str,
) -> bool:
    if p.method is not None and p.method != method:
        return False
    if not p.host_glob.match(host):
        return False
    if p.port is not None and p.port != effective_port:
        return False
    return p.path_glob is None or bool(p.path_glob.match(path))


def resolve_outbound(
    compiled: CompiledTargets | None,
    method: str,
    host: str,
    *,
    scheme: str | None = None,
    port: int | None = None,
    path: str | None = None,
) -> str:
    """The policy target key for one outbound request: exact host key, else the
    most specific matching pattern key (verbatim, so the core's exact lookup
    hits it), else the bare host (class-default fallthrough, as before)."""
    if compiled is None:
        return host
    if host in compiled.exact:
        return host
    if compiled.patterns:
        effective_port = port if port is not None else _SCHEME_PORTS.get(scheme or "")
        host_l = host.lower()
        path_n = path or "/"
        method_u = (method or "GET").upper()
        for p in compiled.patterns:
            if _matches(p, method_u, host_l, effective_port, path_n):
                return p.key
    return host


# --- process-global installation (mirrors `_runtime`'s backend/discovery) ----

_compiled: CompiledTargets | None = None


def install_outbound_targets(policy: Any) -> CompiledTargets:
    """Compile and install the outbound matchers for the effective policy
    (called by bootstrap after `configure`; the same composed document)."""
    global _compiled
    _compiled = compile_outbound_targets(policy)
    return _compiled


def clear_outbound_targets() -> None:
    """Reset to the uninstalled state (uninstall_keel / test teardown)."""
    global _compiled
    _compiled = None


def current_outbound_targets() -> CompiledTargets | None:
    return _compiled
