"""keel-core-stub: an in-memory fake of the keel-core surface (Python form).

Mirrors crates/keel-core-stub semantics exactly; the shared specification
lives in conformance/README.md. Envelopes are plain dicts shaped like the
serde types in contracts/core_api.rs.

Simplifications (deliberate, documented): virtual clock (waits recorded, not
slept), no jitter, consecutive-failure breaker only, fixed-window rate
limiter, exact-match target resolution with defaults.llm / defaults.outbound
fallback, `timeout` validated but not enforced.
"""

from __future__ import annotations

import re
from typing import Any, Callable

ENVELOPE_VERSION = 1

_DEFAULT_SCHEDULE = (200, 2.0, 30_000)  # base_ms, factor, cap_ms
_DEFAULT_ATTEMPTS = 3
_DEFAULT_ON = ["conn", "timeout", "429", "5xx"]
_DEFAULT_BREAKER_FAILURES = 5
_DEFAULT_BREAKER_COOLDOWN_MS = 15_000

_DURATION_RE = re.compile(r"^(\d+)(ms|s|m|h)$")
_DURATION_MULT = {"ms": 1, "s": 1_000, "m": 60_000, "h": 3_600_000}
_RATE_WINDOW = {"s": 1_000, "sec": 1_000, "min": 60_000, "h": 3_600_000, "hour": 3_600_000}
_CLASSES = ("conn", "timeout", "http", "cancelled", "other")


class KeelError(Exception):
    def __init__(self, code: str, message: str):
        super().__init__(f"{code}: {message}")
        self.code = code
        self.message = message


def _parse_duration(s: str) -> int | None:
    m = _DURATION_RE.match(s.strip())
    return int(m.group(1)) * _DURATION_MULT[m.group(2)] if m else None


def _parse_rate(s: str) -> tuple[int, int] | None:
    parts = s.strip().split("/")
    if len(parts) != 2 or not parts[0].strip().isdigit():
        return None
    limit = int(parts[0])
    window = _RATE_WINDOW.get(parts[1].strip())
    return (limit, window) if limit >= 1 and window else None


def _parse_schedule(s: str) -> tuple[int, float, int] | None:
    """Supports the v0.1 primaries exp(base, xF[, max D][, jitter]) and
    fixed(D). Composition (upTo/andThen) is frozen grammar, unimplemented."""
    s = s.strip()
    if s.startswith("exp(") and s.endswith(")"):
        parts = [p.strip() for p in s[4:-1].split(",")]
        if len(parts) < 2:
            return None
        base = _parse_duration(parts[0])
        if base is None or not parts[1].startswith("x"):
            return None
        try:
            factor = float(parts[1][1:])
        except ValueError:
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


def _schedule_wait(sched: tuple[int, float, int], attempt: int) -> int:
    base, factor, cap = sched
    return round(min(base * factor ** (attempt - 1), cap))


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


class KeelCoreStub:
    def __init__(self) -> None:
        self._policy: dict[str, Any] = {}
        self._now_ms = 0
        self._trace_seq = 0
        self._breakers: dict[str, dict[str, Any]] = {}
        self._rate_windows: dict[str, list[int]] = {}
        self._cache: dict[str, tuple[int, Any]] = {}
        self._metrics: dict[str, dict[str, int]] = {}

    # -- configure ---------------------------------------------------------

    def configure(self, policy: dict[str, Any]) -> None:
        if not isinstance(policy, dict):
            raise KeelError("KEEL-E001", "policy invalid at $: policy document must be a table")
        defaults = policy.get("defaults")
        if defaults is not None:
            if not isinstance(defaults, dict):
                raise KeelError("KEEL-E001", "policy invalid at defaults: expected a table")
            for key in ("outbound", "llm"):
                if key in defaults:
                    self._validate_target_policy(f"defaults.{key}", defaults[key])
        targets = policy.get("target")
        if targets is not None:
            if not isinstance(targets, dict):
                raise KeelError("KEEL-E001", "policy invalid at target: expected a table")
            for name, v in targets.items():
                self._validate_target_policy(f'target."{name}"', v)
        self._policy = policy

    @staticmethod
    def _invalid(path: str, msg: str) -> KeelError:
        return KeelError("KEEL-E001", f"policy invalid at {path}: {msg}")

    @classmethod
    def _validate_target_policy(cls, path: str, v: Any) -> None:
        if not isinstance(v, dict):
            raise cls._invalid(path, "expected a table")
        timeout = v.get("timeout")
        if timeout is not None and (
            not isinstance(timeout, str) or _parse_duration(timeout) is None
        ):
            raise cls._invalid(path, "bad timeout duration")
        retry = v.get("retry")
        if retry is not None:
            if not isinstance(retry, dict):
                raise cls._invalid(path, "retry must be a table")
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
                        or (len(c) == 3 and c.isdigit())
                    )
                    if not known:
                        raise cls._invalid(path, "unknown retry.on condition")
        breaker = v.get("breaker")
        if breaker is not None:
            if not isinstance(breaker, dict):
                raise cls._invalid(path, "breaker must be a table")
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
        rate = v.get("rate")
        if rate is not None and (not isinstance(rate, str) or _parse_rate(rate) is None):
            raise cls._invalid(path, "unparseable rate")
        cache = v.get("cache")
        if cache is not None:
            if not isinstance(cache, dict):
                raise cls._invalid(path, "cache must be a table")
            ttl = cache.get("ttl")
            if ttl is not None and (not isinstance(ttl, str) or _parse_duration(ttl) is None):
                raise cls._invalid(path, "bad cache.ttl")

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

        # rate limiter (fixed windows on the virtual clock)
        if isinstance(rate, str):
            limit, window_ms = _parse_rate(rate)
            w = self._now_ms // window_ms
            cell = self._rate_windows.setdefault(target, [w, 0])
            if cell[0] != w:
                cell[0], cell[1] = w, 0
            if cell[1] >= limit:
                nxt = (cell[0] + 1) * window_ms
                out["throttle_wait_ms"] = nxt - self._now_ms
                out["throttled"] = True
                self._now_ms = nxt
                cell[0], cell[1] = nxt // window_ms, 0
                m["throttled"] += 1
            cell[1] += 1

        # breaker check (observes post-retry call outcomes)
        half_open = False
        if isinstance(breaker_cfg, dict):
            b = self._breakers.setdefault(
                target, {"consecutive": 0, "open_until": None, "opens": 0}
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

        # retry loop
        if isinstance(retry, dict):
            max_attempts = retry.get("attempts", _DEFAULT_ATTEMPTS)
            schedule = (
                _parse_schedule(retry["schedule"]) if "schedule" in retry else _DEFAULT_SCHEDULE
            )
            on = retry.get("on", _DEFAULT_ON)
        else:
            max_attempts, schedule, on = 1, _DEFAULT_SCHEDULE, _DEFAULT_ON

        terminal: dict[str, Any] | None = None
        for attempt in range(1, max_attempts + 1):
            out["attempts"] = attempt
            m["attempts"] += 1
            res = effect(attempt)
            if res.get("status") == "ok":
                m["successes"] += 1
                if isinstance(breaker_cfg, dict):
                    b = self._breakers[target]
                    b["consecutive"], b["open_until"] = 0, None
                payload = res.get("payload")
                if cache_key and cache_ttl is not None:
                    self._cache[cache_key] = (self._now_ms + cache_ttl, payload)
                out.update(
                    result="ok", payload=payload, breaker=self._breaker_state(target)
                )
                return out

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
                break
            wait = _schedule_wait(schedule, attempt)
            if res.get("retry_after_ms") is not None:
                wait = max(wait, res["retry_after_ms"])
            out["waits_ms"].append(wait)
            self._now_ms += wait
            m["retries"] += 1

        # terminal failure
        m["failures"] += 1
        if isinstance(breaker_cfg, dict):
            failures = breaker_cfg.get("failures", _DEFAULT_BREAKER_FAILURES)
            cooldown = (
                _parse_duration(breaker_cfg["cooldown"])
                if "cooldown" in breaker_cfg
                else _DEFAULT_BREAKER_COOLDOWN_MS
            )
            b = self._breakers[target]
            if half_open:
                b["open_until"] = self._now_ms + cooldown
                b["opens"] += 1
                b["consecutive"] = 0
            else:
                b["consecutive"] += 1
                if b["consecutive"] >= failures:
                    b["open_until"] = self._now_ms + cooldown
                    b["opens"] += 1
                    b["consecutive"] = 0
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
