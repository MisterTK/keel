"""keel-core-stub: an in-memory fake of the keel-core surface (Python form).

Mirrors crates/keel-core-stub semantics exactly; the shared specification
lives in conformance/README.md. Envelopes are plain dicts shaped like the
serde types in contracts/core_api.rs.

Simplifications (deliberate, documented): virtual clock (waits recorded, not
slept), no jitter, exact-match target resolution with defaults.llm /
defaults.outbound fallback, `timeout` validated but not enforced. Poll is
implemented on the virtual clock too (pending waits advance the clock).

Bit-identical to the real core (parity rule, not a simplification): the
breaker's count mode *and* rate mode (window + failure_rate + min_calls), and
the rate limiter's token bucket (burst = the rate's limit, continuous refill).

Tier 2 parity note: this stub is Tier 1 only — it has no journal and no durable
flows (conformance scenarios 16-17 are skipped on the stub). Durable-flow
time/random *virtualization* (journaled, replayed on resume) is therefore a
native-core-only behavior: an effect run under this stub sees real `time`/
`random`, never the recorded values the native core substitutes on replay. This
is an intentional Tier-2 gap, not a call-path semantics drift — Tier 1 outcomes
stay bit-identical to the native core (that is what the conformance suite pins).
"""

from __future__ import annotations

import base64
import json
import math
import re
from typing import Any, Callable

ENVELOPE_VERSION = 1

_DEFAULT_SCHEDULE = [((200, 2.0, 30_000), None)]  # single unbounded exp segment
_DEFAULT_ATTEMPTS = 3
_DEFAULT_ON = ["conn", "timeout", "429", "5xx"]
_DEFAULT_BREAKER_FAILURES = 5
_DEFAULT_BREAKER_COOLDOWN_MS = 15_000
_DEFAULT_MIN_CALLS = 10

# ASCII digits only (`[0-9]`, not `\d` which also matches unicode digits) and no
# embedded whitespace — matching Node's regexes and the Rust core, so the same
# keel.toml validates identically on every backend.
_DURATION_RE = re.compile(r"^([0-9]+)(ms|s|m|h)$")
_RATE_RE = re.compile(r"^([0-9]+)/(s|sec|min|h|hour)$")
_FACTOR_RE = re.compile(r"^[0-9]+(\.[0-9]+)?$")
# Exact-status retry.on literal: the frozen schema errorCondition grammar
# `[1-5][0-9][0-9]` (100–599), ASCII digits only.
_STATUS_CONDITION_RE = re.compile(r"[1-5][0-9][0-9]")
_DURATION_MULT = {"ms": 1, "s": 1_000, "m": 60_000, "h": 3_600_000}
_RATE_WINDOW = {"s": 1_000, "sec": 1_000, "min": 60_000, "h": 3_600_000, "hour": 3_600_000}
_CLASSES = ("conn", "timeout", "http", "cancelled", "other")

#: Allowed keys per policy object (mirrors contracts/policy.schema.json's
#: additionalProperties:false and the typed core's #[serde(deny_unknown_fields)],
#: so an unknown/typo'd key fails loudly with KEEL-E001 on the stub too).
_ALLOWED_TOP = ("defaults", "target", "flows", "journal", "telemetry")
_ALLOWED_DEFAULTS = ("outbound", "llm")
_ALLOWED_TARGET = ("timeout", "retry", "breaker", "rate", "cache", "idempotency", "fallback", "budget", "poll")
_ALLOWED_RETRY = ("attempts", "schedule", "on")
_ALLOWED_BREAKER = ("failures", "cooldown", "window", "failure_rate", "min_calls")
_ALLOWED_CACHE = ("ttl", "scope", "mode", "key")
_ALLOWED_FLOWS = ("entrypoints", "on_nondeterminism")
_ALLOWED_TELEMETRY = ("otlp_endpoint", "console")


class KeelError(Exception):
    def __init__(self, code: str, message: str):
        super().__init__(f"{code}: {message}")
        self.code = code
        self.message = message


def _parse_duration(s: str) -> int | None:
    m = _DURATION_RE.match(s.strip())
    return int(m.group(1)) * _DURATION_MULT[m.group(2)] if m else None


def _parse_rate(s: str) -> tuple[int, int] | None:
    m = _RATE_RE.match(s.strip())  # anchored/ASCII: rejects "3 / s", unicode digits
    if not m:
        return None
    limit = int(m.group(1))
    window = _RATE_WINDOW[m.group(2)]
    return (limit, window) if limit >= 1 else None


def _parse_primary(s: str) -> tuple[int, float, int] | None:
    """A schedule primary: exp(base, xF[, max D][, jitter]) or fixed(D)."""
    s = s.strip()
    if s.startswith("exp(") and s.endswith(")"):
        parts = [p.strip() for p in s[4:-1].split(",")]
        if len(parts) < 2:
            return None
        base = _parse_duration(parts[0])
        if base is None or not parts[1].startswith("x"):
            return None
        # Factor must be a plain ASCII decimal — reject "xinf"/"xnan"/"x1_0"/
        # unicode, matching Node's Number()+isFinite (float() would accept them).
        factor_str = parts[1][1:]
        if not _FACTOR_RE.match(factor_str):
            return None
        factor = float(factor_str)
        if not math.isfinite(factor):
            return None
        cap = 2**63
        for p in parts[2:]:
            if p.startswith("max "):
                cap = _parse_duration(p[4:].strip())
                if cap is None:
                    return None
            elif p != "jitter":
                return None
        return (base, factor, cap)
    if s.startswith("fixed(") and s.endswith(")"):
        d = _parse_duration(s[6:-1])
        return (d, 1.0, d) if d is not None else None
    return None


def _primary_wait(primary: tuple[int, float, int], local_attempt: int) -> int:
    base, factor, cap = primary
    natural = min(base * factor ** (local_attempt - 1), cap)
    # Round-half-AWAY-FROM-ZERO, matching Rust's `f64::round()` (the real
    # core) and JS's `Math.round()` (the Node stub) exactly — Python's
    # builtin `round()` is round-half-to-even ("banker's rounding"), which
    # disagrees with both on an exact .5ms boundary (e.g. a natural wait of
    # 4.5ms: `round(4.5) == 4` in Python, but 5 everywhere else). Waits here
    # are always non-negative, so `floor(x + 0.5)` is the correct, simple
    # equivalent.
    return math.floor(natural + 0.5)


# A schedule segment: (primary, up_to_ms). `up_to_ms` is the `upTo` bound on
# the segment's cumulative *natural* wait, or None on an unbounded segment.
_ScheduleSegment = tuple[tuple[int, float, int], int | None]


def _parse_schedule(s: str) -> list[_ScheduleSegment] | None:
    """Full grammar (contracts/schedule-grammar.ebnf): one or more `andThen`-
    separated segments, each a primary with an optional cumulative-wait
    `upTo` bound. Semantics pinned in conformance/README.md ("Schedule
    algebra"): every segment but the last must be bounded, and the last never
    is — both degenerate shapes are rejected here so a schedule is always a
    total mapping attempt -> wait."""
    tokens = s.split()
    if not tokens:
        return None
    groups: list[list[str]] = [[]]
    for token in tokens:
        if token == "andThen":
            groups.append([])
        else:
            groups[-1].append(token)
    segments: list[_ScheduleSegment] = []
    for group in groups:
        if not group:
            return None
        up_to_ms: int | None = None
        primary_tokens = group
        if "upTo" in group:
            pos = group.index("upTo")
            rest = group[pos + 1 :]
            if len(rest) != 1:
                return None
            up_to_ms = _parse_duration(rest[0])
            if up_to_ms is None:
                return None
            primary_tokens = group[:pos]
        if not primary_tokens:
            return None
        primary = _parse_primary(" ".join(primary_tokens))
        if primary is None:
            return None
        segments.append((primary, up_to_ms))
    last = len(segments) - 1
    if any((i < last) != (up_to is not None) for i, (_, up_to) in enumerate(segments)):
        return None
    return segments


def _schedule_wait(segments: list[_ScheduleSegment], attempt: int) -> int:
    """Deterministic wait after failed attempt `n` (1-based): walk the
    segments left to right, handing off to the next segment when the active
    one's cumulative natural wait would exceed its `upTo` bound. Mirrors
    crates/keel-core-api's `Schedule::wait_and_jitter` exactly; see
    conformance/README.md ("Schedule algebra") for the normative spec."""
    attempt = max(attempt, 1)
    last = len(segments) - 1
    i, a, e = 0, 1, 0
    emitted = 0
    while True:
        primary, up_to_ms = segments[i]
        wait = _primary_wait(primary, a)
        if i < last and up_to_ms is not None and e + wait > up_to_ms:
            i, a, e = i + 1, 1, 0
            continue
        emitted += 1
        if emitted == attempt:
            return wait
        a += 1
        e += wait


def _breaker_mode(cfg: dict[str, Any]) -> tuple[str, Any]:
    """The mode `cfg` (a resolved `breaker` table) selects, with defaults
    applied — mirrors `BreakerPolicy::mode` in crates/keel-core-api. Returns
    `("count", failures)` or `("rate", (window_ms, failure_rate, min_calls))`.
    Configure-time validation already rejected a half-configured rate mode, so
    `window`/`failure_rate` are either both present or both irrelevant here.
    """
    failures = cfg.get("failures")
    window = cfg.get("window")
    failure_rate = cfg.get("failure_rate")
    if failures is None and window is not None and failure_rate is not None:
        min_calls = cfg.get("min_calls", _DEFAULT_MIN_CALLS)
        return ("rate", (_parse_duration(window), failure_rate, min_calls))
    return ("count", failures if failures is not None else _DEFAULT_BREAKER_FAILURES)


def _breaker_observe(b: dict[str, Any], now_ms: int, window_ms: int, failed: bool) -> None:
    """Rate mode: prune outcomes that aged out of the window (strictly: one
    exactly `window_ms` old is evicted, per conformance/README.md §4), then
    record this one."""
    outcomes: list[tuple[int, bool]] = b["outcomes"]
    while outcomes and now_ms - outcomes[0][0] >= window_ms:
        outcomes.pop(0)
    outcomes.append((now_ms, failed))


def _breaker_window_rate_reached(b: dict[str, Any], failure_rate: float, min_calls: int) -> bool:
    """Rate mode's trip condition over the (already-pruned) window."""
    outcomes: list[tuple[int, bool]] = b["outcomes"]
    total = len(outcomes)
    if total < min_calls:
        return False
    failed = sum(1 for _, f in outcomes if f)
    return failed / total >= failure_rate


def _token_bucket_admit(cell: dict[str, int], now_ms: int, limit: int, window_ms: int) -> int:
    """Token bucket: burst capacity `limit`, continuous refill of `limit` per
    `window_ms`. Bit-identical to the real core's `TokenBucket::plan_admit`
    (crates/keel-core/src/engine.rs) — same fixed-point integer arithmetic (1
    token = `window_ms` scaled units), so no float drift between languages.
    Returns the wait to admit this call (0 = immediate); the caller advances
    the virtual clock by it, standing in for the real core's actual sleep."""
    capacity = limit * window_ms
    if not cell["primed"]:
        cell["primed"] = True
        cell["scaled_tokens"] = capacity
        cell["last_refill_ms"] = now_ms
    elapsed = max(0, now_ms - cell["last_refill_ms"])
    cell["last_refill_ms"] = max(cell["last_refill_ms"], now_ms)
    cell["scaled_tokens"] = min(capacity, cell["scaled_tokens"] + elapsed * limit)
    cell["scaled_tokens"] -= window_ms
    if cell["scaled_tokens"] >= 0:
        return 0
    deficit = -cell["scaled_tokens"]
    return -(-deficit // limit)  # ceil(deficit / limit) via floor-division negation


def _condition_matches(cond: str, cls: str, http_status: int | None) -> bool:
    if cond in ("conn", "timeout", "cancelled", "other"):
        return cond == cls
    if cls != "http" or http_status is None:
        return False
    if cond == "4xx":
        return 400 <= http_status <= 499
    if cond == "5xx":
        return 500 <= http_status <= 599
    return len(cond) == 3 and cond.isdigit() and int(cond) == http_status


def _poll_verdict(poll: dict[str, Any], payload: Any) -> str:
    """Judge one successful iteration's payload: "terminal" | "pending" |
    "fail_open". Parity with keel-core's ``poll_verdict``
    (conformance/README.md "Poll")."""
    if not isinstance(payload, dict):
        return "fail_open"
    doc = payload
    if "body_b64" in payload or ("status" in payload and "headers" in payload):
        b64 = payload.get("body_b64")
        if not isinstance(b64, str):
            return "fail_open"
        try:
            parsed = json.loads(base64.b64decode(b64))
        except Exception:
            return "fail_open"
        if not isinstance(parsed, dict):
            return "fail_open"
        doc = parsed
    field = poll["until"]["field"]
    if field not in doc:
        return "fail_open"
    value = doc[field]
    if isinstance(value, str) and value in poll["until"]["terminal"]:
        return "terminal"
    return "pending"


class KeelCoreStub:
    def __init__(self) -> None:
        self._policy: dict[str, Any] = {}
        self._now_ms = 0
        self._trace_seq = 0
        self._breakers: dict[str, dict[str, Any]] = {}
        self._token_buckets: dict[str, dict[str, int]] = {}
        self._cache: dict[str, tuple[int, Any]] = {}
        self._metrics: dict[str, dict[str, int]] = {}

    # -- configure ---------------------------------------------------------

    def configure(self, policy: dict[str, Any]) -> None:
        if not isinstance(policy, dict):
            raise KeelError("KEEL-E001", "policy invalid at $: policy document must be a table")
        self._reject_unknown("", policy, _ALLOWED_TOP)
        defaults = policy.get("defaults")
        if defaults is not None:
            if not isinstance(defaults, dict):
                raise KeelError("KEEL-E001", "policy invalid at defaults: expected a table")
            self._reject_unknown("defaults", defaults, _ALLOWED_DEFAULTS)
            for key in ("outbound", "llm"):
                if key in defaults:
                    self._validate_target_policy(f"defaults.{key}", defaults[key])
        targets = policy.get("target")
        if targets is not None:
            if not isinstance(targets, dict):
                raise KeelError("KEEL-E001", "policy invalid at target: expected a table")
            for name, v in targets.items():
                self._validate_target_policy(f'target."{name}"', v)
        self._validate_flows(policy.get("flows"))
        self._validate_journal(policy.get("journal"))
        self._validate_telemetry(policy.get("telemetry"))
        self._policy = policy

    @staticmethod
    def _invalid(path: str, msg: str) -> KeelError:
        return KeelError("KEEL-E001", f"policy invalid at {path}: {msg}")

    @staticmethod
    def _reject_unknown(path: str, table: dict[str, Any], allowed: tuple[str, ...]) -> None:
        """Reject any key outside `allowed` (parity with the frozen schema's
        additionalProperties:false + the core's deny_unknown_fields), so a typo
        like `retrys`/`atempts` fails loudly at configure instead of silently
        running the defaults."""
        for k in table:
            if k not in allowed:
                where = f"{path}.{k}" if path else k
                raise KeelCoreStub._invalid(where, f"unknown key {k!r}")

    def _validate_flows(self, flows: Any) -> None:
        if flows is None:
            return
        if not isinstance(flows, dict):
            raise self._invalid("flows", "expected a table")
        self._reject_unknown("flows", flows, _ALLOWED_FLOWS)
        entrypoints = flows.get("entrypoints")
        if entrypoints is not None and (
            not isinstance(entrypoints, list) or not all(isinstance(e, str) for e in entrypoints)
        ):
            raise self._invalid("flows.entrypoints", "must be an array of strings")
        on = flows.get("on_nondeterminism")
        if on is not None and on not in ("fail", "warn", "branch"):
            raise self._invalid("flows.on_nondeterminism", "must be fail|warn|branch")

    def _validate_journal(self, journal: Any) -> None:
        if journal is None:
            return
        if not isinstance(journal, str) or not re.match(r"^(file:.+|postgres://.+)$", journal):
            raise self._invalid("journal", "must be a file:… or postgres://… location")

    def _validate_telemetry(self, telemetry: Any) -> None:
        if telemetry is None:
            return
        if not isinstance(telemetry, dict):
            raise self._invalid("telemetry", "expected a table")
        self._reject_unknown("telemetry", telemetry, _ALLOWED_TELEMETRY)
        endpoint = telemetry.get("otlp_endpoint")
        if endpoint is not None and not isinstance(endpoint, str):
            raise self._invalid("telemetry.otlp_endpoint", "must be a string")
        console = telemetry.get("console")
        if console is not None and not isinstance(console, bool):
            raise self._invalid("telemetry.console", "must be a boolean")

    @classmethod
    def _validate_target_policy(cls, path: str, v: Any) -> None:
        if not isinstance(v, dict):
            raise cls._invalid(path, "expected a table")
        cls._reject_unknown(path, v, _ALLOWED_TARGET)
        timeout = v.get("timeout")
        if timeout is not None and (
            not isinstance(timeout, str) or _parse_duration(timeout) is None
        ):
            raise cls._invalid(path, "bad timeout duration")
        retry = v.get("retry")
        if retry is not None:
            if not isinstance(retry, dict):
                raise cls._invalid(path, "retry must be a table")
            cls._reject_unknown(f"{path}.retry", retry, _ALLOWED_RETRY)
            attempts = retry.get("attempts")
            if attempts is not None and (
                not isinstance(attempts, int) or isinstance(attempts, bool) or attempts < 1
            ):
                raise cls._invalid(path, "retry.attempts must be an integer >= 1")
            schedule = retry.get("schedule")
            if schedule is not None and (
                not isinstance(schedule, str) or _parse_schedule(schedule) is None
            ):
                raise cls._invalid(path, "unparseable retry.schedule")
            on = retry.get("on")
            if on is not None:
                if not isinstance(on, list):
                    raise cls._invalid(path, "retry.on must be an array")
                for c in on:
                    known = isinstance(c, str) and (
                        c in ("conn", "timeout", "cancelled", "other", "4xx", "5xx")
                        # Frozen schema errorCondition grammar: [1-5][0-9][0-9]
                        # (100–599), ASCII digits only. `str.isdigit()` also
                        # matched unicode digits and any 3-digit value (099/999).
                        or _STATUS_CONDITION_RE.fullmatch(c) is not None
                    )
                    if not known:
                        raise cls._invalid(path, "unknown retry.on condition")
        breaker = v.get("breaker")
        if breaker is not None:
            if not isinstance(breaker, dict):
                raise cls._invalid(path, "breaker must be a table")
            cls._reject_unknown(f"{path}.breaker", breaker, _ALLOWED_BREAKER)
            failures = breaker.get("failures")
            if failures is not None and (
                not isinstance(failures, int) or isinstance(failures, bool) or failures < 1
            ):
                raise cls._invalid(path, "breaker.failures must be an integer >= 1")
            cooldown = breaker.get("cooldown")
            if cooldown is not None and (
                not isinstance(cooldown, str) or _parse_duration(cooldown) is None
            ):
                raise cls._invalid(path, "bad breaker.cooldown")
            window = breaker.get("window")
            if window is not None and (
                not isinstance(window, str) or _parse_duration(window) is None
            ):
                raise cls._invalid(path, "bad breaker.window")
            failure_rate = breaker.get("failure_rate")
            if failure_rate is not None and (
                isinstance(failure_rate, bool)
                or not isinstance(failure_rate, (int, float))
                or not (failure_rate > 0.0 and failure_rate <= 1.0)
            ):
                raise cls._invalid(
                    path, "breaker.failure_rate must be greater than 0 and at most 1"
                )
            min_calls = breaker.get("min_calls")
            if min_calls is not None and (
                not isinstance(min_calls, int) or isinstance(min_calls, bool) or min_calls < 1
            ):
                raise cls._invalid(path, "breaker.min_calls must be an integer >= 1")
            # Two modes (frozen schema `$defs/breaker`): "Setting `failures`
            # selects count mode." Otherwise a rate-mode knob without BOTH
            # `window` and `failure_rate` is half-configured — reject it rather
            # than silently running count mode on its defaults.
            has_rate_pair = window is not None and failure_rate is not None
            has_any_rate_knob = (
                window is not None or failure_rate is not None or min_calls is not None
            )
            if failures is None and has_any_rate_knob and not has_rate_pair:
                raise cls._invalid(
                    path,
                    "breaker rate mode requires both `window` and `failure_rate` "
                    "(count mode sets `failures` instead)",
                )
        rate = v.get("rate")
        if rate is not None and (not isinstance(rate, str) or _parse_rate(rate) is None):
            raise cls._invalid(path, "unparseable rate")
        cache = v.get("cache")
        if cache is not None:
            if not isinstance(cache, dict):
                raise cls._invalid(path, "cache must be a table")
            cls._reject_unknown(f"{path}.cache", cache, _ALLOWED_CACHE)
            ttl = cache.get("ttl")
            if ttl is not None and (not isinstance(ttl, str) or _parse_duration(ttl) is None):
                raise cls._invalid(path, "bad cache.ttl")
            # Closed enums (parity with the core's serde enums): a typo like
            # scope="persistant" must fail, not silently fall back to a default.
            scope = cache.get("scope")
            if scope is not None and scope not in ("memory", "persistent"):
                raise cls._invalid(path, "cache.scope must be memory|persistent")
            mode = cache.get("mode")
            if mode is not None and mode not in ("always", "dev"):
                raise cls._invalid(path, "cache.mode must be always|dev")
            key = cache.get("key")
            if key is not None and key not in ("args", "url"):
                raise cls._invalid(path, "cache.key must be args|url")
        idempotency = v.get("idempotency")
        if idempotency is not None:
            if not isinstance(idempotency, dict):
                raise cls._invalid(path, "idempotency must be a table")
            cls._reject_unknown(f"{path}.idempotency", idempotency, ("header",))
            header = idempotency.get("header")
            if not isinstance(header, str):  # header is required by the schema
                raise cls._invalid(path, "idempotency.header must be a string")
        fallback = v.get("fallback")
        if fallback is not None and (
            not isinstance(fallback, list) or not all(isinstance(x, str) for x in fallback)
        ):
            raise cls._invalid(path, "fallback must be an array of strings")
        budget = v.get("budget")
        if budget is not None and not isinstance(budget, str):
            raise cls._invalid(path, "budget must be a string")
        poll = v.get("poll")
        if poll is not None:
            if not isinstance(poll, dict):
                raise cls._invalid(path, "poll must be a table")
            cls._reject_unknown(f"{path}.poll", poll, ("interval", "deadline", "until"))
            for key in ("interval", "deadline"):
                if not isinstance(poll.get(key), str) or _parse_duration(poll[key]) is None:
                    raise cls._invalid(path, f"bad poll.{key} duration")
            until = poll.get("until")
            if not isinstance(until, dict):
                raise cls._invalid(path, "poll.until must be a table")
            cls._reject_unknown(f"{path}.poll.until", until, ("field", "terminal"))
            field = until.get("field")
            if not isinstance(field, str) or not field:
                raise cls._invalid(path, "poll.until.field must be a non-empty string")
            terminal = until.get("terminal")
            if (
                not isinstance(terminal, list)
                or not terminal
                or not all(isinstance(t, str) for t in terminal)
            ):
                raise cls._invalid(path, "poll.until.terminal must be a non-empty array of strings")

    # -- resolution --------------------------------------------------------

    def _layer(self, target: str, key: str) -> Any:
        t = self._policy.get("target", {})
        if isinstance(t, dict) and target in t and key in t[target]:
            return t[target][key]
        defaults = self._policy.get("defaults", {})
        if target.startswith("llm:"):
            llm = defaults.get("llm", {})
            if key in llm:
                return llm[key]
        return defaults.get("outbound", {}).get(key)

    def layer(self, target: str, key: str) -> Any:
        """Public accessor for a resolved policy layer (front ends read this to
        honor `idempotency.header` and gate cache buffering). Parity with Node's
        `AsyncEngine.layer`."""
        return self._layer(target, key)

    # -- execution ---------------------------------------------------------

    def _met(self, target: str) -> dict[str, int]:
        return self._metrics.setdefault(
            target,
            {
                "calls": 0,
                "attempts": 0,
                "retries": 0,
                "successes": 0,
                "failures": 0,
                "cache_hits": 0,
                "throttled": 0,
            },
        )

    def _breaker_state(self, target: str) -> str:
        b = self._breakers.get(target)
        if b and b["open_until"] is not None and self._now_ms < b["open_until"]:
            return "open"
        return "closed"

    def execute(
        self,
        request: dict[str, Any],
        effect: Callable[[int], dict[str, Any]],
    ) -> dict[str, Any]:
        target = request["target"]
        op = request.get("op", target)
        m = self._met(target)
        m["calls"] += 1
        self._trace_seq += 1
        out: dict[str, Any] = {
            "v": ENVELOPE_VERSION,
            "result": "error",
            "attempts": 0,
            "from_cache": False,
            "waits_ms": [],
            "throttled": False,
            "throttle_wait_ms": 0,
            "breaker": "closed",
            "trace_id": f"t-{self._trace_seq:06d}",
        }

        if request.get("v") != ENVELOPE_VERSION:
            out["error"] = {
                "code": "KEEL-E004",
                "class": "other",
                "message": f"unsupported envelope version {request.get('v')}",
            }
            m["failures"] += 1
            return out

        retry = self._layer(target, "retry")
        breaker_cfg = self._layer(target, "breaker")
        rate = self._layer(target, "rate")
        cache_cfg = self._layer(target, "cache")
        cache_ttl = (
            _parse_duration(cache_cfg["ttl"])
            if isinstance(cache_cfg, dict) and "ttl" in cache_cfg
            else None
        )

        # cache (outermost layer)
        args_hash = request.get("args_hash")
        cache_key = f"{target}#{args_hash}" if cache_ttl is not None and args_hash else None
        if cache_key and cache_key in self._cache:
            expires, payload = self._cache[cache_key]
            if self._now_ms < expires:
                m["cache_hits"] += 1
                m["successes"] += 1
                out.update(
                    result="ok",
                    payload=payload,
                    from_cache=True,
                    breaker=self._breaker_state(target),
                )
                return out

        # rate limiter (token bucket: burst + continuous refill)
        if isinstance(rate, str):
            limit, window_ms = _parse_rate(rate)
            cell = self._token_buckets.setdefault(
                target, {"scaled_tokens": 0, "last_refill_ms": 0, "primed": False}
            )
            wait = _token_bucket_admit(cell, self._now_ms, limit, window_ms)
            if wait > 0:
                out["throttle_wait_ms"] = wait
                out["throttled"] = True
                self._now_ms += wait
                m["throttled"] += 1

        # breaker check (observes post-retry call outcomes)
        half_open = False
        if isinstance(breaker_cfg, dict):
            b = self._breakers.setdefault(
                target,
                {"consecutive": 0, "open_until": None, "opens": 0, "outcomes": []},
            )
            if b["open_until"] is not None:
                if self._now_ms < b["open_until"]:
                    out["error"] = {
                        "code": "KEEL-E012",
                        "class": "other",
                        "message": f"breaker OPEN for {target}: failed fast, call not attempted",
                    }
                    out["breaker"] = "open"
                    m["failures"] += 1
                    return out
                half_open = True

        # retry loop config
        if isinstance(retry, dict):
            max_attempts = retry.get("attempts", _DEFAULT_ATTEMPTS)
            schedule = (
                _parse_schedule(retry["schedule"]) if "schedule" in retry else _DEFAULT_SCHEDULE
            )
            on = retry.get("on", _DEFAULT_ON)
        else:
            max_attempts, schedule, on = 1, _DEFAULT_SCHEDULE, _DEFAULT_ON

        def run_attempts() -> tuple[str, Any]:
            """One fully-retried call: ("ok", payload) or ("error", terminal)."""
            for attempt in range(1, max_attempts + 1):
                out["attempts"] += 1
                m["attempts"] += 1
                res = effect(attempt)
                if res.get("status") == "ok":
                    return ("ok", res.get("payload"))
                cls = res.get("class", "other")
                http_status = res.get("http_status")
                message = res.get("message", "")
                retryable = any(_condition_matches(c, cls, http_status) for c in on)
                if not retryable:
                    code = "KEEL-E015"
                elif attempt == max_attempts:
                    code = "KEEL-E010"
                elif not request.get("idempotent", False):
                    code = "KEEL-E014"
                else:
                    code = None
                if code:
                    detail = f"{cls} {http_status}" if http_status else cls
                    if code == "KEEL-E010":
                        msg = f"{op} failed {attempt}/{max_attempts} attempts (last: {detail}). {message}"
                    elif code == "KEEL-E014":
                        msg = (
                            f"{op} failed ({detail}). Not retried: call is not idempotent "
                            f"— observed, not retried. {message}"
                        )
                    else:
                        msg = f"{op} failed ({detail}); error class is not retryable per policy. {message}"
                    terminal = {"code": code, "class": cls, "message": msg.rstrip()}
                    if http_status is not None:
                        terminal["http_status"] = http_status
                    if res.get("original") is not None:
                        terminal["original"] = res["original"]
                    return ("error", terminal)
                wait = _schedule_wait(schedule, attempt)
                if res.get("retry_after_ms") is not None:
                    wait = max(wait, res["retry_after_ms"])
                out["waits_ms"].append(wait)
                self._now_ms += wait
                m["retries"] += 1
            raise AssertionError("loop always returns by the final attempt")

        # poll layer (CCR-3): wraps the retry loop; gate = resolved poll table
        # + idempotent GET/HEAD op (conformance/README.md "Poll").
        poll_cfg = self._layer(target, "poll")
        poll_active = (
            isinstance(poll_cfg, dict)
            and request.get("idempotent", False)
            and (op.startswith("GET ") or op.startswith("HEAD "))
        )
        poll_started_ms = self._now_ms
        while True:
            status_, value = run_attempts()
            if status_ == "ok" and poll_active:
                verdict = _poll_verdict(poll_cfg, value)
                if verdict == "pending":
                    interval = _parse_duration(poll_cfg["interval"])
                    deadline = _parse_duration(poll_cfg["deadline"])
                    elapsed = self._now_ms - poll_started_ms
                    if elapsed + interval > deadline:
                        status_, value = "error", {
                            "code": "KEEL-E016",
                            "class": "other",
                            "message": (
                                f"{op} poll deadline exceeded: "
                                f"'{poll_cfg['until']['field']}' not terminal after {deadline}ms"
                            ),
                        }
                        break
                    self._now_ms += interval
                    continue
            break

        if status_ == "ok":
            payload = value
            m["successes"] += 1
            if isinstance(breaker_cfg, dict):
                b = self._breakers[target]
                closed_a_probe = b["open_until"] is not None
                b["consecutive"], b["open_until"] = 0, None
                if closed_a_probe:
                    b["outcomes"] = []
                else:
                    mode, params = _breaker_mode(breaker_cfg)
                    if mode == "rate":
                        window_ms, _, _ = params
                        _breaker_observe(b, self._now_ms, window_ms, False)
            if cache_key and cache_ttl is not None:
                self._cache[cache_key] = (self._now_ms + cache_ttl, payload)
            out.update(result="ok", payload=payload, breaker=self._breaker_state(target))
            return out

        # terminal failure (unchanged bookkeeping)
        terminal = value
        m["failures"] += 1
        if isinstance(breaker_cfg, dict):
            cooldown = (
                _parse_duration(breaker_cfg["cooldown"])
                if "cooldown" in breaker_cfg
                else _DEFAULT_BREAKER_COOLDOWN_MS
            )
            b = self._breakers[target]
            if half_open:
                should_trip = True  # failed probe: re-open for another full cooldown
            else:
                mode, params = _breaker_mode(breaker_cfg)
                if mode == "count":
                    b["consecutive"] += 1
                    should_trip = b["consecutive"] >= params
                else:
                    window_ms, failure_rate, min_calls = params
                    _breaker_observe(b, self._now_ms, window_ms, True)
                    should_trip = _breaker_window_rate_reached(b, failure_rate, min_calls)
            if should_trip:
                b["open_until"] = self._now_ms + cooldown
                b["opens"] += 1
                b["consecutive"] = 0
                b["outcomes"] = []
        out["error"] = terminal
        out["breaker"] = self._breaker_state(target)
        return out

    # -- reporting ---------------------------------------------------------

    def report(self) -> dict[str, Any]:
        targets = {}
        for name in sorted(self._metrics):
            m = self._metrics[name]
            b = self._breakers.get(name, {})
            targets[name] = {
                "attempts": m["attempts"],
                "breaker_opens": b.get("opens", 0),
                "breaker_state": self._breaker_state(name),
                "cache_hits": m["cache_hits"],
                "calls": m["calls"],
                "failures": m["failures"],
                "retries": m["retries"],
                "successes": m["successes"],
                "throttled": m["throttled"],
            }
        return {"v": 1, "clock_ms": self._now_ms, "targets": targets}

    def advance_clock(self, ms: int) -> None:
        self._now_ms += ms


__all__ = ["KeelCoreStub", "KeelError", "ENVELOPE_VERSION"]
