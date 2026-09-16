//! The Python static scan: an `ast`-walker executed out-of-process via
//! `python3 -`.
//!
//! Parsing Python with Python's own `ast` is exact where a regex would guess:
//! it sees real imports and string-literal constants with true line numbers.
//! The walker script is embedded and fed on stdin; it prints one JSON object.
//! If `python3` is absent the pass yields nothing and reports
//! [`available`](PyScan::available)`= false` so the caller can say so out loud
//! rather than silently under-reporting coverage.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use serde::Deserialize;

use super::{
    DepAverseFile, FunctionFacts, LangFindings, Sighting, SimplificationSighting,
    SubprocessSighting, TransportClass,
};

/// The embedded `ast` walker. Deterministic: directories and files are visited
/// in sorted order, output keys are sorted. Finds imports of the known effect
/// libraries and URL/DSN string literals, each with `file:line` — and, for
/// `keel flows suggest`, attributes effect / time / random / replay-unsafe
/// calls to their enclosing **module-level** function defs (real AST
/// containment: nested defs and lambdas inside a function count toward it;
/// class methods are not flow entrypoints and are not attributed).
const AST_WALKER: &str = r#"
import ast, ipaddress, json, os, re, sys
from urllib.parse import urlsplit

HTTP_LIBS = {"httpx", "requests", "aiohttp", "urllib3", "urllib.request"}
LLM_LIBS = {"openai", "anthropic", "google.genai"}
OTHER_LIBS = {"psycopg", "boto3"}
# The agent-framework packs (dx-spec agent-first-class work). pydantic_ai,
# crewai, langgraph, and openai-agents (import name `agents`) are plain
# top-level module names, classified exactly like HTTP_LIBS/LLM_LIBS/
# OTHER_LIBS above. `mcp` is the SDK's own import name (mcp_pack) — a normal
# adapter lib here, not special-cased. `google.adk` is handled separately via
# dotted-prefix matching (see `import_entries`): bare `google` is a namespace
# package shared with unrelated distributions (google-protobuf and friends)
# and must record nothing.
AGENT_LIBS = {"pydantic_ai", "crewai", "langgraph", "agents", "mcp"}
KNOWN = HTTP_LIBS | LLM_LIBS | OTHER_LIBS | AGENT_LIBS | {"google.adk"}
# SDK "poll the job" calls, per provider, as attribute-chain suffixes. A
# function that contains one of these while its module imports that provider
# reaches the provider's target even when the client handle is untracked
# (built by a factory, imported from elsewhere) — the shape that made the
# 2026-09-15 field report's poll loop invisible (#95).
SDK_POLL_METHODS = {
    "google.genai": {("operations", "get")},
    "openai": {("batches", "retrieve"), ("videos", "retrieve"), ("fine_tuning", "jobs", "retrieve")},
    "anthropic": {("batches", "retrieve")},
}
# Known Python resilience libraries — a `keel doctor` signal that a target may
# already have its own retry/backoff, separate from KNOWN (which is about
# libraries Keel *adapts*; these are libraries Keel never adapts, so mixing
# them into KNOWN would misclassify them as an "invisible" coverage gap).
RESILIENCE_LIBS = {"tenacity", "backoff", "retrying", "stamina"}
TIME_LIBS = {"time", "datetime"}
RANDOM_LIBS = {"random", "uuid", "secrets"}
UNSAFE_LIBS = {"threading", "multiprocessing", "subprocess", "socket"}
# Transport classification for `keel doctor`: a URL sighting is "tracked" when
# a registry-adapted library is in reach in the same file — including stdlib
# `urllib.request`, adapted since v0.3.0 (its dotted key lives in HTTP_LIBS;
# see import_entries) — "untracked-known" when only an unadapted stdlib
# transport is (http.client, or urllib without the request submodule), and
# otherwise "unknown" — no reachable transport at all.
STDLIB_TRANSPORTS = {"urllib", "http"}  # http.client / non-request urllib
TRACKED_TRANSPORTS = HTTP_LIBS | {"psycopg", "boto3"}
TRACKED = KNOWN | TIME_LIBS | RANDOM_LIBS | UNSAFE_LIBS | STDLIB_TRANSPORTS | {"os", "asyncio"}
TIME_NAMES = {"time", "time_ns", "monotonic", "monotonic_ns",
              "perf_counter", "perf_counter_ns", "gmtime", "localtime"}
DT_NAMES = {"now", "utcnow", "today"}
UUID_NAMES = {"uuid1", "uuid3", "uuid4", "uuid5"}
OS_UNSAFE = {"system", "popen", "fork", "forkpty", "execv", "execve",
             "execvp", "execvpe", "spawnl", "spawnv", "spawnvp"}
# Subprocess-launching call names itemized (with literal argv where
# extractable) as their own finer-grained signal, separate from the coarse
# `unsafe_reasons` UNSAFE_LIBS/OS_UNSAFE already feed.
SUBPROC_NAMES = {"run", "Popen", "call", "check_call", "check_output"}
# Hand-rolled-resilience pattern detection (simplification leads). Sleep is
# the common denominator: time.sleep / asyncio.sleep. BROAD_EXC is the
# silent-swallow trigger — `except:` bare or Exception/BaseException; a
# narrow exception tuple is deliberate error handling, not a swallow.
SLEEP_LIBS = {"time", "asyncio"}
BROAD_EXC = {"Exception", "BaseException"}
# `sys.stdlib_module_names` exists on Python 3.10+ (keel's floor); the
# `getattr` default degrades older interpreters to "never stdlib-only"
# (an empty STDLIB) rather than crashing or false-positiving.
STDLIB = set(getattr(sys, "stdlib_module_names", ()))
DEP_AVERSE_RE = re.compile(r"(gate|guard|auth|valid|safety|risk|kill)", re.I)
SKIP = {".keel", ".git", "__pycache__", "node_modules", ".venv", "venv",
        ".mypy_cache", ".pytest_cache", "dist", "build", "target"}
# The two `google` submodules Keel adapts. Bare `import google` (the
# namespace package) records nothing — it also hosts unrelated distributions
# (google-protobuf and friends) that are none of Keel's business.
GOOGLE_SUBMODULES = {"adk", "genai"}


def top(mod):
    return mod.split(".", 1)[0] if mod else ""


def import_entries(node):
    """Yield (module_key, bound_name) for each name an Import/ImportFrom node
    binds. Two dotted-prefix special cases resolve to a dotted key instead of
    the top-level name: `google.adk`/`google.genai` (bare `google` is a
    namespace package and records nothing) and `urllib.request` (the adapted
    stdlib transport — `import urllib.request`, `from urllib import request`,
    and `from urllib.request import ...` all resolve to `urllib.request`;
    other urllib submodules keep the plain `urllib` key)."""
    if isinstance(node, ast.Import):
        for alias in node.names:
            parts = alias.name.split(".")
            if len(parts) >= 2 and parts[0] == "google" and parts[1] in GOOGLE_SUBMODULES:
                key = "google." + parts[1]
            elif len(parts) >= 2 and parts[0] == "urllib" and parts[1] == "request":
                key = "urllib.request"
            else:
                key = parts[0]
            yield key, (alias.asname or alias.name).split(".", 1)[0]
    elif isinstance(node, ast.ImportFrom):
        mod = node.module or ""
        mparts = mod.split(".")
        if len(mparts) >= 2 and mparts[0] == "google" and mparts[1] in GOOGLE_SUBMODULES:
            key = "google." + mparts[1]
            for alias in node.names:
                yield key, alias.asname or alias.name
        elif mod == "google":
            for alias in node.names:
                if alias.name in GOOGLE_SUBMODULES:
                    yield "google." + alias.name, alias.asname or alias.name
        elif len(mparts) >= 2 and mparts[0] == "urllib" and mparts[1] == "request":
            for alias in node.names:
                yield "urllib.request", alias.asname or alias.name
        elif mod == "urllib":
            for alias in node.names:
                key = "urllib.request" if alias.name == "request" else "urllib"
                yield key, alias.asname or alias.name
        else:
            key = top(mod)
            for alias in node.names:
                yield key, alias.asname or alias.name


def plausible(h):
    if h == "localhost":
        return True
    if any(c in h for c in "{}$%()<>"):
        return False
    try:
        ipaddress.ip_address(h)
        return True
    except ValueError:
        pass
    if "." not in h:
        return False
    return all(
        label
        and all(c.isalnum() or c == "-" for c in label)
        and not label.startswith("-")
        and not label.endswith("-")
        for label in h.split(".")
    )


def host(s):
    if "://" not in s:
        return None
    try:
        parts = urlsplit(s.strip())
    except ValueError:
        return None
    if not parts.scheme or not parts.hostname:
        return None
    # hostname is already lowercased by urlsplit.
    return parts.hostname if plausible(parts.hostname) else None


def call_root(f):
    """The Name at the base of a call's attribute chain, or None. Deliberately
    does NOT see through intermediate calls: in `httpx.get(u).json()` only the
    inner `httpx.get(u)` has a root, so a chained method on a call result is
    never double-counted as a second effect."""
    while isinstance(f, ast.Attribute):
        f = f.value
    return f.id if isinstance(f, ast.Name) else None


# Call names that construct a handle rather than perform an effect: CapWords
# constructors (OpenAI(), Client()) plus the well-known factory methods.
FACTORY_NAMES = {"client", "resource", "session", "connect"}


def is_constructor(name):
    return name is None or name[:1].isupper() or name in FACTORY_NAMES


def aliases_of(tree):
    """Binding name -> tracked module (top-level, or `google.adk`/
    `google.genai` — see `import_entries`). Also follows one hop of
    constructor assignment (client = OpenAI() -> client is an openai handle),
    the dominant SDK-client pattern. A later assignment that rebinds such a
    name to anything that is NOT another known-library handle (a literal, an
    unrelated constructor, any other call) invalidates it, so a generic name
    (`client`, `session`, `resp`) bound to a library in one place and reused
    for something else elsewhere in the file is not misattributed to the first
    library. Import bindings themselves are never invalidated — module names
    like `os`/`time`/`subprocess` are effectively never rebound, and the
    file-wide subprocess/sleep passes rely on them staying put. Known misses of
    this flat, unscoped, single-snapshot dict (no scoping/branch modeling here,
    not worth chasing): two functions binding one name to *different* known
    libs still collide last-write-wins; invalidation only fires when the known
    binding precedes the reuse in traversal order; and invalidating a shared
    name also drops the original binder's own handle (conservative — favor a
    missed attribution over a wrong one)."""
    a = {}
    imported = set()
    for node in ast.walk(tree):
        if isinstance(node, (ast.Import, ast.ImportFrom)):
            for key, bound in import_entries(node):
                if key in TRACKED:
                    a[bound] = key
                    imported.add(bound)
    for node in ast.walk(tree):
        if not isinstance(node, ast.Assign):
            continue
        lib = a.get(call_root(node.value.func)) if isinstance(node.value, ast.Call) else None
        for tgt in node.targets:
            if not isinstance(tgt, ast.Name):
                continue
            if lib in KNOWN:
                a[tgt.id] = lib
            elif tgt.id not in imported:
                a.pop(tgt.id, None)
    return a


def url_consts_of(tree):
    """Module-level NAME = "scheme://host/..." constants -> host, so a URL
    hoisted to a constant still attributes to the functions that use it."""
    consts = {}
    for node in tree.body:
        if (isinstance(node, ast.Assign) and isinstance(node.value, ast.Constant)
                and isinstance(node.value.value, str)):
            h = host(node.value.value)
            if h:
                for tgt in node.targets:
                    if isinstance(tgt, ast.Name):
                        consts[tgt.id] = h
    return consts


def argv_text(call):
    """The launched command as literal text, when statically extractable: a
    bare string arg, or a list/tuple of string-literal elements (the argv
    shape). Anything else (a variable, an f-string, a partial list) is
    genuinely dynamic — report it as such rather than guessing."""
    if call.args:
        a = call.args[0]
        if isinstance(a, ast.Constant) and isinstance(a.value, str):
            return a.value
        if isinstance(a, (ast.List, ast.Tuple)) and a.elts and all(
                isinstance(e, ast.Constant) and isinstance(e.value, str)
                for e in a.elts):
            return " ".join(e.value for e in a.elts)
    return "<dynamic>"


def argv_list(call):
    """The launched argv as a genuine positional list, or ``None`` (issue
    #41). Only a list/tuple of string-literal elements, with no ``shell=True``
    keyword, is this shape — the exact condition
    ``subprocess_pack.py``'s runtime interceptor requires to consider a call
    for ``[flows.match."cmd:*"]`` dispatch (``_str_argv``/the ``shell``
    check). A bare string command (``os.system``, ``shell=True``) is a
    DIFFERENT launch shape the runtime pack never matches; ``None`` here is
    doctor's signal that this sighting can never be a match candidate,
    regardless of what its text happens to look like."""
    for kw in call.keywords:
        if kw.arg == "shell" and isinstance(kw.value, ast.Constant) and kw.value.value is True:
            return None
    if call.args:
        a = call.args[0]
        if isinstance(a, (ast.List, ast.Tuple)) and a.elts and all(
                isinstance(e, ast.Constant) and isinstance(e.value, str)
                for e in a.elts):
            return [e.value for e in a.elts]
    return None


PY_LAUNCHERS = {"python", "python3", "uv", "uvx", "pipx", "poetry", "pdm", "hatch"}
NODE_LAUNCHERS = {"node", "npx", "tsx", "npm", "pnpm", "yarn", "bun", "deno"}


def _first_argv(call):
    """The first argv element as a literal string, or the sentinel
    "sys.executable" when it is that expression, else None."""
    if not call.args:
        return None
    a = call.args[0]
    if isinstance(a, ast.Constant) and isinstance(a.value, str):
        parts = a.value.split()
        return parts[0] if parts else None
    if isinstance(a, (ast.List, ast.Tuple)) and a.elts:
        e = a.elts[0]
        if isinstance(e, ast.Constant) and isinstance(e.value, str):
            return e.value
        if (isinstance(e, ast.Attribute) and isinstance(e.value, ast.Name)
                and e.value.id == "sys" and e.attr == "executable"):
            return "sys.executable"
    return None


def child_runtime(call):
    """"python" / "node" when the launched program is recognizably one of
    those runtimes (a bare interpreter name, a versioned `python3.12`, a
    Python/Node package runner, or `sys.executable`), else None (WS7)."""
    first = _first_argv(call)
    if first is None:
        return None
    if first == "sys.executable":
        return "python"
    base = first.rsplit("/", 1)[-1].rsplit("\\", 1)[-1]
    if base.endswith(".exe"):
        base = base[:-4]
    if base in PY_LAUNCHERS or base.startswith("python3.") or base.startswith("python2."):
        return "python"
    if base in NODE_LAUNCHERS:
        return "node"
    return None


def _is_os_environ(node):
    return (isinstance(node, ast.Attribute) and isinstance(node.value, ast.Name)
            and node.value.id == "os" and node.attr == "environ")


def env_mode(call):
    """How the child's environment relates to ours: "inherited" (no `env=`,
    `env=os.environ`, `{**os.environ, …}`, `dict(os.environ, …)`),
    "replaced" (a dict literal that does not spread os.environ), or
    "unknown" (any other expression). Decides whether KEEL_ENABLE reaches
    the child (WS7)."""
    for kw in call.keywords:
        if kw.arg != "env":
            continue
        v = kw.value
        if _is_os_environ(v):
            return "inherited"
        if isinstance(v, ast.Dict):
            spreads = any(k is None and _is_os_environ(val) for k, val in zip(v.keys, v.values))
            return "inherited" if spreads else "replaced"
        if (isinstance(v, ast.Call) and isinstance(v.func, ast.Name) and v.func.id == "dict"
                and v.args and _is_os_environ(v.args[0])):
            return "inherited"
        return "unknown"
    return "inherited"


def is_sleep(node, aliases):
    """Known conservative miss: `from time import sleep as pause` binds
    `pause` to the "time" MODULE in `aliases` (`import_entries` only tracks
    module -> binding, never the specific imported symbol name), so a bare
    `pause(...)` call falls into the `ast.Name` branch below with
    `name = "pause"` — never `"sleep"` — and is missed. Renaming the sleep
    call itself (as opposed to renaming the time/asyncio module, which IS
    tracked via `aliases`) defeats detection. Not worth chasing for a
    synthetic edge case."""
    if not isinstance(node, ast.Call):
        return False
    lib = aliases.get(call_root(node.func))
    name = node.func.attr if isinstance(node.func, ast.Attribute) else call_root(node.func)
    return lib in SLEEP_LIBS and name == "sleep"


TIMEOUT_NAME_HINTS = ("timeout", "deadline", "elapsed", "waited", "max_wait",
                      "max_seconds")


def _int_seconds(node):
    """A positive WHOLE-second literal, or None. `True`/`False` are ints in
    Python and are never seconds; a fractional literal (`sleep(0.5)`) is not a
    whole number of seconds and yields None rather than a rounded guess — the
    proposal falls back to its documented default when this is None."""
    if not isinstance(node, ast.Constant) or isinstance(node.value, bool):
        return None
    v = node.value
    if not isinstance(v, int) or not 0 < v < 2 ** 31:
        return None
    return v


def _timeout_named(name):
    """A name that plausibly holds a timeout/deadline IN SECONDS. A sub-second
    unit in the name (`timeout_ms`, `elapsed_millis`) disqualifies it outright:
    the literal beside it is not a number of seconds, and reading it as one
    would propose a deadline 1000x too long."""
    lowered = name.lower()
    if any(u in lowered for u in ("_ms", "milli", "micro", "_us", "nano")):
        return False
    return any(h in lowered for h in TIMEOUT_NAME_HINTS)


def _mentions_timeout_name(expr):
    return any((isinstance(n, ast.Name) and _timeout_named(n.id))
               or (isinstance(n, ast.Attribute) and _timeout_named(n.attr))
               for n in ast.walk(expr))


def loop_interval_s(node, aliases):
    """The loop's OWN sleep interval in whole seconds — but only when every
    sleep call in the loop names the same integer literal. A computed backoff
    (`time.sleep(backoff)`), a fractional sleep, a bare `sleep()`, or two
    sleeps that disagree all yield None: #107 says read the loop's literals,
    never guess a number."""
    vals = set()
    for sub in ast.walk(node):
        if not is_sleep(sub, aliases):
            continue
        if not sub.args:
            return None
        v = _int_seconds(sub.args[0])
        if v is None:
            return None
        vals.add(v)
    return vals.pop() if len(vals) == 1 else None


def fn_deadline_s(fn):
    """A whole-second deadline the FUNCTION itself states, when it states
    exactly one: the default of a timeout-named parameter (`timeout_s: int =
    900`) or a comparison of a timeout-named expression against a literal
    (`if elapsed > 900:`). Zero candidates, or two that disagree, yield None —
    an ambiguous deadline is not a deadline Keel knows."""
    vals = set()
    a = fn.args
    positional = list(getattr(a, "posonlyargs", [])) + list(a.args)
    if a.defaults:
        for arg, default in zip(positional[len(positional) - len(a.defaults):], a.defaults):
            if _timeout_named(arg.arg):
                v = _int_seconds(default)
                if v is not None:
                    vals.add(v)
    for arg, default in zip(a.kwonlyargs, a.kw_defaults):
        if default is not None and _timeout_named(arg.arg):
            v = _int_seconds(default)
            if v is not None:
                vals.add(v)
    for n in ast.walk(fn):
        if not (isinstance(n, ast.Compare) and len(n.comparators) == 1):
            continue
        left, right = n.left, n.comparators[0]
        for lit, other in ((left, right), (right, left)):
            v = _int_seconds(lit)
            if v is not None and _mentions_timeout_name(other):
                vals.add(v)
    return vals.pop() if len(vals) == 1 else None


def is_broad_handler(h):
    if h.type is None:
        return True
    names = h.type.elts if isinstance(h.type, ast.Tuple) else [h.type]
    return any(isinstance(n, ast.Name) and n.id in BROAD_EXC for n in names)


def is_default_return(stmt):
    """`return`, `return None/<constant>`, or `return {}/[]/()` — the shapes
    that silently substitute a default for a failure."""
    if not isinstance(stmt, ast.Return):
        return False
    v = stmt.value
    if v is None or isinstance(v, ast.Constant):
        return True
    if isinstance(v, ast.Dict):
        return not v.keys
    if isinstance(v, (ast.List, ast.Tuple)):
        return not v.elts
    return False


def _test_has_string_compare(test):
    """Does an expression subtree (an `if`/`while` test) contain a
    comparison against a string constant?"""
    return any(
        isinstance(n, ast.Compare) and any(
            isinstance(c, ast.Constant) and isinstance(c.value, str)
            for c in [n.left] + list(n.comparators))
        for n in ast.walk(test))


def _governs_status_cmp(node):
    """A string-comparison compare that actually governs whether the loop
    exits: the loop's own `while` condition, or a nested `if` whose body
    breaks or returns. Restricting to these (rather than ANY string compare
    found anywhere inside the loop) avoids mislabeling a hand-rolled retry
    loop as a poll loop merely because some unrelated string comparison
    happens to sit deeper in the body — the wrong recommended fix (`poll`
    policy instead of retry/backoff) would follow from that mislabel."""
    if isinstance(node, ast.While) and _test_has_string_compare(node.test):
        return True
    return any(
        isinstance(sub, ast.If)
        and any(isinstance(s, (ast.Break, ast.Return)) for s in sub.body)
        and _test_has_string_compare(sub.test)
        for sub in ast.walk(node))


def _assigned_name(target):
    """The bound name of an assignment target, for the narrow shapes an SDK
    poll result is realistically stored in: a plain name (`op = ...`) or an
    attribute (`self.op = ...`, keyed by its final attribute). Anything else
    (tuple/list unpacking, subscript targets) returns None — deliberately
    not treated as a poll-governing binding."""
    if isinstance(target, ast.Name):
        return target.id
    if isinstance(target, ast.Attribute):
        return target.attr
    return None


def _sdk_poll_feeds_exit(node, imported_llm_libs):
    """An SDK "poll the job" call only counts as poll evidence when its
    RESULT governs whether the loop exits — mirroring the restriction
    `_governs_status_cmp` already applies to string comparisons, for the
    same reason: a status-sync loop that iterates DISTINCT jobs and merely
    calls an SDK poll-shaped method per item (e.g. `for id in ids: b =
    client.batches.retrieve(id); cache[id] = b; sleep(...)`) is not a poll
    of one job and must not be flagged. Governance is: the call's result is
    bound to a name/attribute, and that name is referenced in either the
    loop's own `while` test, or the test of a nested `if` whose body breaks
    or returns. #101: a call written INLINE in that same test needs no bound
    name at all — it IS the condition."""

    def sdk_poll_in(expr):
        # #101: the call sits directly inside a loop-exit test —
        # `while not client.operations.get(op).done:` — so it IS the
        # condition; governance needs no bound name.
        return any(isinstance(n, ast.Call) and sdk_poll_provider(n, imported_llm_libs) is not None
                   for n in ast.walk(expr))

    if isinstance(node, ast.While) and sdk_poll_in(node.test):
        return True
    if any(isinstance(sub, ast.If)
           and any(isinstance(s, (ast.Break, ast.Return)) for s in sub.body)
           and sdk_poll_in(sub.test)
           for sub in ast.walk(node)):
        return True
    bound = set()
    for sub in ast.walk(node):
        if not (isinstance(sub, ast.Assign) and isinstance(sub.value, ast.Call)
                and sdk_poll_provider(sub.value, imported_llm_libs) is not None):
            continue
        for t in sub.targets:
            name = _assigned_name(t)
            if name:
                bound.add(name)
    if not bound:
        return False

    def refs(expr):
        return any(
            (isinstance(n, ast.Name) and n.id in bound)
            or (isinstance(n, ast.Attribute) and n.attr in bound)
            for n in ast.walk(expr))

    if isinstance(node, ast.While) and refs(node.test):
        return True
    return any(
        isinstance(sub, ast.If)
        and any(isinstance(s, (ast.Break, ast.Return)) for s in sub.body)
        and refs(sub.test)
        for sub in ast.walk(node))


def detect_simplifications(fn, aliases, imported_llm_libs):
    """The three WS3 patterns inside one function def, each anchored at the
    construct to delete. Precedence inside a loop: a status-string comparison
    makes it a poll (the stronger, WS5-pairing signal) even if an attempt
    counter is also present. An SDK "poll the job" call whose result governs
    the loop's exit (see `_sdk_poll_feeds_exit`) is the same strong signal by
    itself — the adopter's `while not op.done: ... op =
    client.operations.get(op)` loop has no status-string comparison anywhere
    in it, but the SDK shape alone says "polling a job" (#95). An
    except-handler sleep inside an already-matched loop is the same
    construct, not a second finding."""
    found = []
    covered = set()
    deadline_s = fn_deadline_s(fn)
    for node in ast.walk(fn):
        if not isinstance(node, (ast.While, ast.For, ast.AsyncFor)):
            continue
        has_sleep = has_counter = False
        for sub in ast.walk(node):
            if is_sleep(sub, aliases):
                has_sleep = True
            elif isinstance(sub, ast.AugAssign) and isinstance(sub.op, ast.Add):
                has_counter = True
            elif (isinstance(sub, ast.Assign) and isinstance(sub.value, ast.BinOp)
                    and isinstance(sub.value.op, ast.Add)
                    and isinstance(sub.value.left, ast.Name)
                    and len(sub.targets) == 1
                    and isinstance(sub.targets[0], ast.Name)
                    and sub.targets[0].id == sub.value.left.id):
                has_counter = True
        if not has_sleep:
            continue
        if _governs_status_cmp(node) or _sdk_poll_feeds_exit(node, imported_llm_libs):
            kind = "hand-rolled-poll"
        elif has_counter:
            kind = "hand-rolled-retry"
        else:
            continue
        shapes = sorted({sdk_poll_shape(sub, imported_llm_libs)
                         for sub in ast.walk(node) if isinstance(sub, ast.Call)} - {None})
        found.append({"kind": kind, "line": node.lineno,
                      "sdk_polls": shapes if kind == "hand-rolled-poll" else [],
                      "interval_s": loop_interval_s(node, aliases),
                      "deadline_s": deadline_s})
        covered.update(id(sub) for sub in ast.walk(node))
    for node in ast.walk(fn):
        if (isinstance(node, ast.ExceptHandler) and id(node) not in covered
                and any(is_sleep(sub, aliases) for sub in ast.walk(node))):
            found.append({"kind": "hand-rolled-retry", "line": node.lineno, "sdk_polls": []})
    for node in ast.walk(fn):
        if not isinstance(node, ast.Try):
            continue
        calls_target = any(
            isinstance(sub, ast.Call)
            and aliases.get(call_root(sub.func)) in KNOWN | STDLIB_TRANSPORTS
            for stmt in node.body for sub in ast.walk(stmt))
        if not calls_target:
            continue
        for h in node.handlers:
            if (is_broad_handler(h) and len(h.body) == 1
                    and is_default_return(h.body[0])):
                found.append({"kind": "silent-swallow", "line": h.lineno, "sdk_polls": []})
    return found


def attr_chain(node):
    """`a.b.c(...)` -> ("a", "b", "c"); None for anything that is not a plain
    Name/Attribute chain."""
    parts = []
    while isinstance(node, ast.Attribute):
        parts.append(node.attr)
        node = node.value
    if not isinstance(node, ast.Name):
        return None
    parts.append(node.id)
    return tuple(reversed(parts))


def _sdk_poll_match(call, imported_llm_libs):
    """(lib, suffix) for an SDK "poll the job" call, or None."""
    chain = attr_chain(call.func)
    if chain is None:
        return None
    for lib in sorted(imported_llm_libs):
        for suffix in sorted(SDK_POLL_METHODS.get(lib, ())):
            if len(chain) > len(suffix) and chain[-len(suffix):] == suffix:
                return (lib, suffix)
    return None


def sdk_poll_provider(call, imported_llm_libs):
    m = _sdk_poll_match(call, imported_llm_libs)
    return m[0] if m else None


def sdk_poll_shape(call, imported_llm_libs):
    """The dotted method suffix a poll call matched (`"operations.get"`,
    `"fine_tuning.jobs.retrieve"`), or None — what `keel doctor` turns into
    a route-key `poll` proposal."""
    m = _sdk_poll_match(call, imported_llm_libs)
    return ".".join(m[1]) if m else None


def fn_facts(fn, rel, mod, aliases, url_consts, imported_llm_libs):
    effects = unsafe_idem = t_reads = r_reads = 0
    targets = set()
    reasons = []
    for node in ast.walk(fn):
        if isinstance(node, ast.Name) and node.id in url_consts:
            targets.add(url_consts[node.id])
        elif isinstance(node, ast.Constant) and isinstance(node.value, str):
            h = host(node.value)
            if h:
                targets.add(h)
        if not isinstance(node, ast.Call):
            continue
        provider = sdk_poll_provider(node, imported_llm_libs)
        if provider is not None:
            targets.add("llm:" + provider.replace(".", "-"))
        lib = aliases.get(call_root(node.func))
        if lib is None:
            continue
        attr = node.func.attr if isinstance(node.func, ast.Attribute) else None
        name = attr if attr is not None else call_root(node.func)
        if lib in KNOWN:
            if is_constructor(name):
                continue  # a handle being built, not an effect performed
            effects += 1
            if name in {"post", "patch"}:
                unsafe_idem += 1
            if lib in LLM_LIBS:
                targets.add("llm:" + lib.replace(".", "-"))
        elif lib == "time" and name in TIME_NAMES:
            t_reads += 1
        elif lib == "datetime" and name in DT_NAMES:
            t_reads += 1
        elif lib in {"random", "secrets"}:
            r_reads += 1
        elif lib == "uuid" and name in UUID_NAMES:
            r_reads += 1
        elif lib == "os" and name == "urandom":
            r_reads += 1
        elif lib in UNSAFE_LIBS:
            reasons.append((node.lineno, "%s use at %s:%d" % (lib, rel, node.lineno)))
        elif lib == "os" and name in OS_UNSAFE:
            reasons.append((node.lineno, "os.%s at %s:%d" % (name, rel, node.lineno)))
    return {"effects": effects, "file": rel, "idempotent_unsafe": unsafe_idem,
            "line": fn.lineno, "module": mod, "name": fn.name,
            "random_reads": r_reads, "targets": sorted(targets),
            "time_reads": t_reads,
            "unsafe_reasons": [t for _, t in sorted(reasons)]}


root = sys.argv[1] if len(sys.argv) > 1 else "."
imports = []
urls = []
subprocesses = []
simplifications = []
dependency_averse = []
functions = []
resilience_libs = set()
files = 0
for dirpath, dirnames, filenames in os.walk(root):
    dirnames[:] = sorted(d for d in dirnames if d not in SKIP and not d.startswith("."))
    for fn in sorted(filenames):
        if not fn.endswith(".py"):
            continue
        path = os.path.join(dirpath, fn)
        rel = os.path.relpath(path, root).replace(os.sep, "/")
        try:
            with open(path, "r", encoding="utf-8") as fh:
                src = fh.read()
            tree = ast.parse(src)
        except (OSError, SyntaxError, UnicodeDecodeError, ValueError):
            continue
        files += 1
        file_tracked = set()
        file_stdlib = set()
        file_urls = []
        for node in ast.walk(tree):
            if isinstance(node, (ast.Import, ast.ImportFrom)):
                for key, _bound in import_entries(node):
                    if key in KNOWN:
                        imports.append({"lib": key, "file": rel, "line": node.lineno})
                    elif key in RESILIENCE_LIBS:
                        resilience_libs.add(key)
                    if key in TRACKED_TRANSPORTS:
                        file_tracked.add(key)
                    elif key in STDLIB_TRANSPORTS:
                        file_stdlib.add(key)
            elif isinstance(node, ast.Constant) and isinstance(node.value, str):
                h = host(node.value)
                if h:
                    file_urls.append({"host": h, "file": rel, "line": node.lineno})
        for u in file_urls:
            if file_tracked:
                u["transport"], u["via"] = "tracked", sorted(file_tracked)[0]
            elif file_stdlib:
                u["transport"], u["via"] = "untracked-known", sorted(file_stdlib)[0]
            else:
                u["transport"], u["via"] = "unknown", None
            urls.append(u)
        mod = rel[:-3].replace("/", ".")
        if mod.endswith(".__init__"):
            mod = mod[: -len(".__init__")]
        aliases = aliases_of(tree)
        for node in ast.walk(tree):
            if not isinstance(node, ast.Call):
                continue
            lib = aliases.get(call_root(node.func))
            name = node.func.attr if isinstance(node.func, ast.Attribute) else call_root(node.func)
            if lib == "subprocess" and name in SUBPROC_NAMES:
                subprocesses.append({"file": rel, "line": node.lineno,
                                     "launcher": "subprocess." + name,
                                     "command": argv_text(node),
                                     "argv": argv_list(node),
                                     "child_runtime": child_runtime(node),
                                     "env": env_mode(node)})
            elif lib == "os" and name in ("system", "popen"):
                subprocesses.append({"file": rel, "line": node.lineno,
                                     "launcher": "os." + name,
                                     "command": argv_text(node),
                                     "argv": None,
                                     "child_runtime": child_runtime(node),
                                     "env": env_mode(node)})
        consts = url_consts_of(tree)
        imported_llm_libs = {lib for lib in set(aliases.values()) if lib in LLM_LIBS}
        for node in tree.body:
            if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
                facts = fn_facts(node, rel, mod, aliases, consts, imported_llm_libs)
                functions.append(facts)
                # Conservative emission gate (WS3 spec): only functions that
                # also reach a Keel-relevant target get simplification
                # sightings — a sleep loop around purely local work is none
                # of Keel's business.
                if facts["targets"]:
                    for hit in detect_simplifications(node, aliases, imported_llm_libs):
                        simplifications.append({
                            "file": rel, "function": node.name,
                            "kind": hit["kind"], "line": hit["line"],
                            "targets": facts["targets"],
                            "sdk_polls": hit.get("sdk_polls", []),
                            "interval_s": hit.get("interval_s"),
                            "deadline_s": hit.get("deadline_s")})

        # Dependency-averse detection: stdlib-only files with a risk/gate/
        # guard/auth/valid/safety/kill name or docstring signal, or an
        # explicit marker. Markers win in both directions: `# keel: exclude`
        # forces the classification regardless of imports; `# keel: include`
        # defeats the heuristic even if it would otherwise match. `node.level
        # == 0` excludes relative imports (`from .x import y`) from counting
        # as third-party — they are always project-internal.
        third_party = False
        for node in ast.walk(tree):
            mods = []
            if isinstance(node, ast.Import):
                mods = [top(a.name) for a in node.names]
            elif isinstance(node, ast.ImportFrom) and node.level == 0:
                mods = [top(node.module or "")]
            if any(m and m not in STDLIB for m in mods):
                third_party = True
                break
        if '# keel: exclude' in src:
            dependency_averse.append({"file": rel, "reason": "marker"})
        elif '# keel: include' not in src and not third_party and STDLIB:
            doc = ast.get_docstring(tree) or ""
            m = DEP_AVERSE_RE.search(rel.rsplit("/", 1)[-1]) or DEP_AVERSE_RE.search(doc)
            if m:
                dependency_averse.append(
                    {"file": rel,
                     "reason": "stdlib-only + name/docstring signal: " + m.group(1).lower()})

subprocesses.sort(key=lambda x: (x["file"], x["line"]))
simplifications.sort(key=lambda x: (x["file"], x["line"], x["kind"]))
dependency_averse.sort(key=lambda x: x["file"])
print(json.dumps({"dependency_averse": dependency_averse, "files_scanned": files,
                  "functions": functions, "imports": imports,
                  "resilience_libs": sorted(resilience_libs),
                  "simplifications": simplifications,
                  "subprocesses": subprocesses, "urls": urls}, sort_keys=True))
"#;

/// One import finding from the walker.
#[derive(Debug, Deserialize)]
struct Import {
    lib: String,
    file: String,
    line: u32,
}

/// One URL-literal finding from the walker.
#[derive(Debug, Deserialize)]
struct Url {
    host: String,
    file: String,
    line: u32,
    /// `"tracked"` / `"untracked-known"` / absent-or-anything-else = unknown.
    /// The walker's `via` (which stdlib/tracked lib classified it) is not
    /// consumed on the Rust side yet — only the class matters for
    /// `host_transports` — so it is left for serde to ignore rather than
    /// carried as an unread field.
    #[serde(default)]
    transport: Option<String>,
}

/// One module-level function's facts from the walker.
#[derive(Debug, Deserialize)]
struct PyFunction {
    effects: u32,
    file: String,
    idempotent_unsafe: u32,
    line: u32,
    module: String,
    name: String,
    random_reads: u32,
    targets: Vec<String>,
    time_reads: u32,
    unsafe_reasons: Vec<String>,
}

/// One subprocess/external-process launch from the walker.
#[derive(Debug, Deserialize)]
struct WalkerSubprocess {
    file: String,
    line: u32,
    launcher: String,
    command: String,
    /// The literal argv as a positional vector, or `None` when the call is
    /// not the "list/tuple of string literals, no `shell=True`" shape
    /// `subprocess_pack.py`'s runtime interceptor requires (issue #41) —
    /// `argv_list`'s doc explains why this is a stricter, DIFFERENT
    /// condition than `command`'s "statically extractable at all".
    #[serde(default)]
    argv: Option<Vec<String>>,
    /// `"python"` / `"node"` / `None` — see [`SubprocessSighting::child_runtime`].
    #[serde(default)]
    child_runtime: Option<String>,
    /// `"inherited"` / `"replaced"` / `"unknown"` — see
    /// [`SubprocessSighting::env_inheritance`].
    env: String,
}

/// One simplification sighting from the walker (see
/// [`SimplificationSighting`]).
#[derive(Debug, Deserialize)]
struct WalkerSimplification {
    file: String,
    function: String,
    kind: String,
    line: u32,
    targets: Vec<String>,
    #[serde(default)]
    sdk_polls: Vec<String>,
    #[serde(default)]
    interval_s: Option<u32>,
    #[serde(default)]
    deadline_s: Option<u32>,
}

/// One dependency-averse file from the walker (see [`DepAverseFile`]).
#[derive(Debug, Deserialize)]
struct WalkerDepAverse {
    file: String,
    reason: String,
}

/// The walker's JSON output, typed.
#[derive(Debug, Deserialize)]
struct WalkerOutput {
    #[serde(default)]
    dependency_averse: Vec<WalkerDepAverse>,
    files_scanned: usize,
    #[serde(default)]
    functions: Vec<PyFunction>,
    imports: Vec<Import>,
    #[serde(default)]
    resilience_libs: Vec<String>,
    #[serde(default)]
    simplifications: Vec<WalkerSimplification>,
    #[serde(default)]
    subprocesses: Vec<WalkerSubprocess>,
    urls: Vec<Url>,
}

/// The Python pass result.
#[derive(Debug, Clone, Default)]
pub struct PyScan {
    /// Whether `python3` ran the walker.
    pub available: bool,
    /// Files the walker parsed.
    pub files_scanned: usize,
    /// Findings, ready to merge.
    pub findings: LangFindings,
    /// Per-function attribution (module-level defs), for `keel flows suggest`.
    pub functions: Vec<FunctionFacts>,
}

const HTTP_LIBS: &[&str] = &["httpx", "requests", "aiohttp", "urllib3", "urllib.request"];

/// Normalize a walker-reported import key to the name `keel doctor`'s
/// `REGISTRY` (see `crate::doctor`) keys its adapters by. The walker emits
/// raw Python identifiers (`google.adk`, `pydantic_ai`, `agents`); doctor's
/// registry — and the docs/findings that reference it — use the packs'
/// PyPI-ish public names (`google-adk`, `pydantic-ai`, `openai-agents`).
/// Everything else passes through unchanged (`crewai`, `langgraph`, `mcp`,
/// `httpx`, `openai`, …).
fn normalize_lib(lib: &str) -> String {
    match lib {
        "google.adk" => "google-adk".to_owned(),
        "google.genai" => "google-genai".to_owned(),
        "pydantic_ai" => "pydantic-ai".to_owned(),
        "agents" => "openai-agents".to_owned(),
        other => other.to_owned(),
    }
}

/// Run the walker over `project`. A missing `python3`, or a walker that fails,
/// yields an empty unavailable result — never a panic.
pub fn scan(project: &Path) -> PyScan {
    let Some(output) = run_walker(project) else {
        return PyScan::default();
    };
    let mut findings = LangFindings::default();
    for imp in &output.imports {
        let lib = normalize_lib(&imp.lib);
        findings.libs.insert(lib.clone());
        let sighting = Sighting {
            file: imp.file.clone(),
            line: imp.line,
        };
        match lib.as_str() {
            "openai" => findings.llm.push(("openai".to_owned(), sighting)),
            "anthropic" => findings.llm.push(("anthropic".to_owned(), sighting)),
            "google-genai" => findings.llm.push(("google-genai".to_owned(), sighting)),
            lib if HTTP_LIBS.contains(&lib) => findings.http_in_use = true,
            // psycopg/boto3/agent-pack libs: recorded as effect libraries via
            // their DSN/URL literals (if any) or the doctor adapter registry;
            // no synthetic host target from the import alone.
            _ => {}
        }
    }
    // A DSN literal (postgres://…) is itself evidence of an outbound call even
    // without one of the HTTP libraries imported.
    if !output.urls.is_empty() {
        findings.http_in_use = true;
    }
    for lib in &output.resilience_libs {
        findings.resilience_libs.insert(lib.clone());
    }
    for sub in output.subprocesses {
        findings.subprocesses.push(SubprocessSighting {
            file: sub.file,
            line: sub.line,
            launcher: sub.launcher,
            command: sub.command,
            argv: sub.argv,
            child_runtime: sub.child_runtime,
            env_inheritance: sub.env,
        });
    }
    for s in output.simplifications {
        findings.simplifications.push(SimplificationSighting {
            file: s.file,
            line: s.line,
            kind: s.kind,
            function: s.function,
            targets: s.targets,
            sdk_polls: s.sdk_polls,
            interval_s: s.interval_s,
            deadline_s: s.deadline_s,
        });
    }
    for d in output.dependency_averse {
        findings.dependency_averse.push(DepAverseFile {
            file: d.file,
            reason: d.reason,
        });
    }
    for url in &output.urls {
        // The walker already returned a bare hostname (urlsplit.hostname), so it
        // is lowercased and port-stripped; normalize defensively.
        let host = url.host.to_ascii_lowercase();
        findings.hosts.push((
            host.clone(),
            Sighting {
                file: url.file.clone(),
                line: url.line,
            },
        ));
        let class = match url.transport.as_deref() {
            Some("tracked") => TransportClass::Tracked,
            Some("untracked-known") => TransportClass::UntrackedKnown,
            _ => TransportClass::Unknown,
        };
        findings
            .host_transports
            .entry(host)
            .and_modify(|c| *c = (*c).min(class))
            .or_insert(class);
    }
    let functions = output
        .functions
        .into_iter()
        .map(|f| FunctionFacts {
            entrypoint: format!("py:{}:{}", f.module, f.name),
            file: f.file,
            line: f.line,
            effects: f.effects,
            idempotent_unsafe: f.idempotent_unsafe,
            time_reads: f.time_reads,
            random_reads: f.random_reads,
            unsafe_reasons: f.unsafe_reasons,
            targets: f.targets.into_iter().collect(),
        })
        .collect();
    PyScan {
        available: true,
        files_scanned: output.files_scanned,
        findings,
        functions,
    }
}

/// Spawn `python3 - <root>`, feed the walker on stdin, parse its stdout.
fn run_walker(project: &Path) -> Option<WalkerOutput> {
    let mut child = Command::new("python3")
        .arg("-")
        .arg(project)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    child.stdin.take()?.write_all(AST_WALKER.as_bytes()).ok()?;
    let out = child.wait_with_output().ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn python3_present() -> bool {
        Command::new("python3")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    /// Issue #29: the tracked-transport set is enumerated in three places —
    /// this file's Rust [`HTTP_LIBS`], the embedded walker's `HTTP_LIBS` set,
    /// and `doctor.rs`'s `REGISTRY`. That shape is inherent, not a bug: the
    /// walker runs out-of-process so its copy must be spelled in real Python,
    /// the Rust copy classifies the walker's JSON back in-process, and REGISTRY
    /// is a richer per-adapter table (its `host`-targeted python rows include
    /// `psycopg`, so it isn't even the same 5-set). None can derive from
    /// another across the process/semantic boundary — so per the issue we guard
    /// the pair that lives in THIS file with a consistency test rather than
    /// collapse it. Pure string-parse, so it runs without `python3`.
    #[test]
    fn rust_and_walker_http_libs_stay_in_sync() {
        let line = AST_WALKER
            .lines()
            .find(|l| l.trim_start().starts_with("HTTP_LIBS = {"))
            .expect("walker defines HTTP_LIBS as a set literal");
        let inner = line
            .split_once('{')
            .and_then(|(_, rest)| rest.split_once('}'))
            .map(|(names, _)| names)
            .expect("HTTP_LIBS set-literal braces");
        let walker: std::collections::BTreeSet<&str> = inner
            .split(',')
            .map(|s| s.trim().trim_matches('"'))
            .filter(|s| !s.is_empty())
            .collect();
        let rust: std::collections::BTreeSet<&str> = HTTP_LIBS.iter().copied().collect();
        assert_eq!(
            walker, rust,
            "Rust HTTP_LIBS and the embedded walker HTTP_LIBS have drifted (issue #29)"
        );
    }

    #[test]
    fn walks_imports_and_url_literals() {
        if !python3_present() {
            eprintln!("skip: python3 not available");
            return;
        }
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("app.py"),
            "import httpx\nfrom openai import OpenAI\n\nURL = \"https://api.example.com/v1\"\n",
        )
        .unwrap();
        let scan = scan(dir.path());
        assert!(scan.available);
        assert_eq!(scan.files_scanned, 1);
        assert!(scan.findings.http_in_use);
        assert!(
            scan.findings
                .llm
                .iter()
                .any(|(p, s)| p == "openai" && s.file == "app.py" && s.line == 2)
        );
        assert!(
            scan.findings
                .hosts
                .iter()
                .any(|(h, s)| h == "api.example.com" && s.line == 4)
        );
    }

    #[test]
    fn implausible_hosts_are_rejected_at_extraction() {
        // Case table shared verbatim with mod.rs's `host_from_url` test — keep
        // in sync.
        if !python3_present() {
            eprintln!("skip: python3 not available");
            return;
        }
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("app.py"),
            "import requests\n\n\
             A = \"s3://bucket/key\"\n\
             B = \"redis://session\"\n\
             C = \"https://{b}/x\".format(b=1)\n\
             D = \"https://api.stripe.com/v1\"\n\
             E = \"http://127.0.0.1:8000\"\n\
             F = \"http://[::1]:9000/x\"\n\
             \n\
             def f():\n    \
             \"\"\"docstring with s3://b/k and gs://artifact/x inside\"\"\"\n",
        )
        .unwrap();
        let scan = scan(dir.path());
        assert!(scan.available);
        let hosts: std::collections::BTreeSet<&str> = scan
            .findings
            .hosts
            .iter()
            .map(|(h, _)| h.as_str())
            .collect();
        assert_eq!(
            hosts,
            std::collections::BTreeSet::from(["api.stripe.com", "127.0.0.1", "::1"]),
            "hosts: {hosts:?}"
        );
    }

    #[test]
    fn urls_are_classified_by_transport() {
        if !python3_present() {
            eprintln!("skip: python3 not available");
            return;
        }
        let dir = TempDir::new().unwrap();
        // tracked: httpx imported in the same file.
        fs::write(
            dir.path().join("a.py"),
            "import httpx\nU = \"https://api.tracked.com/v1\"\n",
        )
        .unwrap();
        // tracked since WS4: urllib.request is an adapted transport.
        fs::write(
            dir.path().join("b.py"),
            "import urllib.request\nU = \"https://api.stdlib.com/v1\"\n",
        )
        .unwrap();
        // unknown: bare URL, no transport at all.
        fs::write(
            dir.path().join("c.py"),
            "U = \"https://api.mystery.com/v1\"\n",
        )
        .unwrap();
        // untracked-known: http.client is still unadapted.
        fs::write(
            dir.path().join("d.py"),
            "import http.client\nU = \"https://api.lowlevel.com/v1\"\n",
        )
        .unwrap();
        // untracked-known: bare urllib (parse only) is not the adapted
        // submodule — still just "stdlib urllib in reach".
        fs::write(
            dir.path().join("e.py"),
            "from urllib import parse\nU = \"https://api.parseonly.com/v1\"\n",
        )
        .unwrap();
        let s = scan(dir.path());
        let t = |h: &str| s.findings.host_transports.get(h).copied();
        assert_eq!(t("api.tracked.com"), Some(TransportClass::Tracked));
        assert_eq!(t("api.stdlib.com"), Some(TransportClass::Tracked));
        assert_eq!(t("api.mystery.com"), Some(TransportClass::Unknown));
        assert_eq!(t("api.lowlevel.com"), Some(TransportClass::UntrackedKnown));
        assert_eq!(t("api.parseonly.com"), Some(TransportClass::UntrackedKnown));
    }

    /// WS4: every import form that puts `urllib.request` in reach resolves to
    /// the dotted lib key `urllib.request` (the google.adk precedent) — it
    /// lands in `libs` (so doctor's REGISTRY row reports detected) and makes
    /// same-file URL sightings tracked.
    #[test]
    fn urllib_request_import_forms_resolve_to_the_dotted_key() {
        if !python3_present() {
            eprintln!("skip: python3 not available");
            return;
        }
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("f1.py"),
            "import urllib.request\nU = \"https://one.example.com/v\"\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("f2.py"),
            "from urllib import request\nU = \"https://two.example.com/v\"\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("f3.py"),
            "from urllib.request import urlopen\nU = \"https://three.example.com/v\"\n",
        )
        .unwrap();
        let s = scan(dir.path());
        assert!(
            s.findings.libs.contains("urllib.request"),
            "{:?}",
            s.findings.libs
        );
        for host in ["one.example.com", "two.example.com", "three.example.com"] {
            assert_eq!(
                s.findings.host_transports.get(host).copied(),
                Some(TransportClass::Tracked),
                "{host}"
            );
        }
    }

    #[test]
    fn agent_pack_imports_and_the_google_dotted_prefix_are_classified() {
        if !python3_present() {
            eprintln!("skip: python3 not available");
            return;
        }
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("app.py"),
            "import google\n\
             import google.protobuf\n\
             from google import protobuf\n\
             import google.adk\n\
             from google.genai import types\n\
             from google import genai, adk\n\
             from pydantic_ai import Agent\n\
             import crewai\n\
             from langgraph.graph import StateGraph\n\
             from agents import Agent as OpenAIAgent\n\
             import mcp\n\
             from tenacity import retry\n",
        )
        .unwrap();
        let scan = scan(dir.path());
        assert!(scan.available);
        // The six agent-framework packs + the two google submodules, all
        // normalized to doctor's REGISTRY names.
        for lib in [
            "google-adk",
            "google-genai",
            "pydantic-ai",
            "crewai",
            "langgraph",
            "openai-agents",
            "mcp",
        ] {
            assert!(scan.findings.libs.contains(lib), "missing lib: {lib}");
        }
        // google.genai joins the llm findings, alongside openai/anthropic.
        assert!(scan.findings.llm.iter().any(|(p, _)| p == "google-genai"));
        // tenacity is resilience, not a coverage-relevant lib.
        assert_eq!(
            scan.findings.resilience_libs,
            ["tenacity".to_owned()].into_iter().collect()
        );
        // Bare `google` (the namespace package) must record NOTHING — it
        // also hosts unrelated distributions (google-protobuf and friends).
        // Neither must `import google.protobuf` nor `from google import
        // protobuf`: `protobuf` isn't one of Keel's two adapted submodules
        // (`GOOGLE_SUBMODULES = {"adk", "genai"}`), so both forms must fall
        // through to the same "namespace package, not evidence" handling —
        // never surfacing as `google`, `google.protobuf`, or `protobuf`.
        for leaked in ["google", "google.protobuf", "protobuf"] {
            assert!(
                !scan.findings.libs.contains(leaked),
                "google.protobuf import must not record {leaked}"
            );
        }
    }

    #[test]
    fn resilience_lib_imports_are_detected_separately_from_known_libs() {
        if !python3_present() {
            eprintln!("skip: python3 not available");
            return;
        }
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("app.py"),
            "import httpx\nfrom tenacity import retry\nimport backoff\n",
        )
        .unwrap();
        let scan = scan(dir.path());
        assert_eq!(
            scan.findings.resilience_libs,
            ["backoff".to_owned(), "tenacity".to_owned()]
                .into_iter()
                .collect()
        );
        // Resilience libs never pollute `libs` (which feeds doctor's
        // "invisible/unadapted effect library" coverage classification —
        // tenacity/backoff were never adapter candidates).
        assert!(!scan.findings.libs.contains("tenacity"));
        assert!(!scan.findings.libs.contains("backoff"));
        assert!(scan.findings.libs.contains("httpx"));
    }

    #[test]
    fn attributes_effects_time_random_to_module_level_functions() {
        if !python3_present() {
            eprintln!("skip: python3 not available");
            return;
        }
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("pipeline.py"),
            r#"import time
import random
import httpx
from openai import OpenAI

API = "https://api.example.com/v1/data"
client = OpenAI()


def main():
    started = time.time()
    seed = random.random()
    data = httpx.get(API).json()
    httpx.post(API, json=data)
    client.responses.create(model="gpt-4.1", input="hi")
    return started, seed


def helper():
    return 41 + 1
"#,
        )
        .unwrap();
        let s = scan(dir.path());
        let main = s
            .functions
            .iter()
            .find(|f| f.entrypoint == "py:pipeline:main")
            .expect("main attributed");
        // get + post + create — the chained .json() must NOT double-count.
        assert_eq!(main.effects, 3);
        assert_eq!(main.idempotent_unsafe, 1, "only the POST");
        assert_eq!(main.time_reads, 1);
        assert_eq!(main.random_reads, 1);
        assert!(main.unsafe_reasons.is_empty());
        assert!(main.targets.contains("api.example.com"), "URL via constant");
        assert!(main.targets.contains("llm:openai"), "client = OpenAI() hop");
        assert_eq!((main.file.as_str(), main.line), ("pipeline.py", 10));
        let helper = s
            .functions
            .iter()
            .find(|f| f.entrypoint == "py:pipeline:helper")
            .expect("helper attributed");
        assert_eq!(helper.effects, 0);
    }

    /// Issue #29: `aliases_of` builds one flat, file-wide binding map, so a
    /// generic handle name (`client`, `session`, `resp`) bound to a tracked
    /// library in one function and then REUSED for an unrelated purpose in
    /// another used to keep its stale mapping — misattributing the second
    /// function's calls to the first library. A non-known reassignment now
    /// invalidates the stale entry.
    #[test]
    fn reused_handle_name_does_not_leak_its_alias_across_functions() {
        if !python3_present() {
            eprintln!("skip: python3 not available");
            return;
        }
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("clients.py"),
            r#"import openai


def build():
    client = openai.OpenAI()
    return client.responses.create(model="gpt-4.1", input="hi")


def unrelated():
    client = LocalThing()
    client.process()
    return client


def direct():
    return openai.responses.create(model="gpt-4.1", input="hi")
"#,
        )
        .unwrap();
        let s = scan(dir.path());
        let f = |name: &str| {
            s.functions
                .iter()
                .find(|f| f.entrypoint == format!("py:clients:{name}"))
                .unwrap_or_else(|| panic!("{name} attributed"))
        };
        // The bug: `unrelated` reuses `client` for a non-openai handle and must
        // NOT inherit build()'s openai attribution (before the fix it did:
        // effects == 1, targets == ["llm:openai"]).
        let unrelated = f("unrelated");
        assert_eq!(unrelated.effects, 0, "reused name misattributed an effect");
        assert!(
            !unrelated.targets.contains("llm:openai"),
            "reused name leaked the openai target: {:?}",
            unrelated.targets
        );
        // Import bindings themselves are never invalidated — a direct
        // `openai.<call>` in a third function is still attributed.
        let direct = f("direct");
        assert_eq!(direct.effects, 1);
        assert!(direct.targets.contains("llm:openai"));
        // Documented flat-dict tradeoff (characterization): invalidating the
        // shared name also clears build()'s own handle, so build loses its
        // attribution rather than risk the collision — conservative by design.
        assert_eq!(f("build").effects, 0);
    }

    #[test]
    fn threads_and_subprocess_defeat_the_replay_safe_estimate() {
        if !python3_present() {
            eprintln!("skip: python3 not available");
            return;
        }
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("jobs.py"),
            r#"import subprocess
import threading
import requests


def risky():
    requests.post("https://api.example.com/v1/x")
    threading.Thread(target=print).start()
    subprocess.run(["ls"])
"#,
        )
        .unwrap();
        let s = scan(dir.path());
        let f = s
            .functions
            .iter()
            .find(|f| f.entrypoint == "py:jobs:risky")
            .expect("risky attributed");
        assert_eq!(f.effects, 1);
        assert_eq!(
            f.unsafe_reasons,
            vec![
                "threading use at jobs.py:8".to_owned(),
                "subprocess use at jobs.py:9".to_owned(),
            ]
        );
    }

    #[test]
    fn subprocess_launches_are_itemized_with_literal_argv() {
        if !python3_present() {
            eprintln!("skip: python3 not available");
            return;
        }
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("launch.py"),
            r#"import subprocess
import os

def go(cmd):
    subprocess.run(["uvx", "alpaca-mcp-server"])
    subprocess.Popen(cmd)
    os.system("./scripts/kill_switch.sh")
"#,
        )
        .unwrap();
        let s = scan(dir.path());
        let items: Vec<(&str, &str)> = s
            .findings
            .subprocesses
            .iter()
            .map(|x| (x.launcher.as_str(), x.command.as_str()))
            .collect();
        assert_eq!(
            items,
            vec![
                ("subprocess.run", "uvx alpaca-mcp-server"),
                ("subprocess.Popen", "<dynamic>"),
                ("os.system", "./scripts/kill_switch.sh"),
            ]
        );
        assert!(
            s.findings
                .subprocesses
                .iter()
                .all(|x| x.file == "launch.py")
        );
    }

    #[test]
    fn subprocess_sightings_carry_child_runtime_and_env_inheritance() {
        if !python3_present() {
            eprintln!("skip: python3 not available");
            return;
        }
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("launch.py"),
            r#"import os, subprocess, sys
subprocess.run([sys.executable, "-m", "amh_media.render"], env={**os.environ, "KEEL_ENABLE": "1"})
subprocess.run(["python3.12", "worker.py"])
subprocess.run(["uv", "run", "job.py"], env={"PATH": "/usr/bin"})
subprocess.run(["node", "svc.mjs"], env=os.environ)
subprocess.run(["ffmpeg", "-i", "in.mp4"])
subprocess.run(["python", "x.py"], env=make_env())
"#,
        )
        .unwrap();
        let s = scan(dir.path());
        let got: Vec<(u32, Option<&str>, &str)> = s
            .findings
            .subprocesses
            .iter()
            .map(|p| {
                (
                    p.line,
                    p.child_runtime.as_deref(),
                    p.env_inheritance.as_str(),
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![
                (2, Some("python"), "inherited"), // sys.executable + {**os.environ, …}
                (3, Some("python"), "inherited"), // python3.12, no env kwarg
                (4, Some("python"), "replaced"),  // uv, env dict without os.environ
                (5, Some("node"), "inherited"),   // env=os.environ
                (6, None, "inherited"),           // ffmpeg
                (7, Some("python"), "unknown"),   // env=make_env()
            ]
        );
    }

    #[test]
    fn hand_rolled_retry_poll_and_swallow_are_detected() {
        if !python3_present() {
            eprintln!("skip: python3 not available");
            return;
        }
        let dir = TempDir::new().unwrap();
        // Poll: the fetch_short_metrics.py:79-95 shape — while + sleep +
        // status-string comparison. The `while` is on line 7 of this file.
        fs::write(
            dir.path().join("poll.py"),
            r#"import time
import urllib.request

API = "https://api.tavily.com/research"

def poller(request_id):
    while time.time() < 90:
        with urllib.request.urlopen(API) as r:
            data = r.read().decode()
        if data == "completed":
            return data
        time.sleep(10)
    return None
"#,
        )
        .unwrap();
        // Retry: loop + manually incremented attempt counter + sleep (the sleep
        // sits in the except handler INSIDE the loop — must yield ONE sighting
        // anchored at the loop, not a second one at the handler).
        fs::write(
            dir.path().join("retry.py"),
            r#"import time
import urllib.request

API = "https://api.alpaca.example/v2"

def retryer():
    attempts = 0
    while True:
        try:
            return urllib.request.urlopen(API)
        except Exception:
            attempts += 1
            time.sleep(1)
"#,
        )
        .unwrap();
        // Swallow: the screen.py:376-398 shape — broad except, single default
        // return, urlopen in the try body.
        fs::write(
            dir.path().join("swallow.py"),
            r#"import urllib.request

BASE = "https://data.alpaca.example"

def fetcher(path):
    try:
        with urllib.request.urlopen(BASE) as r:
            return r.read()
    except Exception:
        return None
"#,
        )
        .unwrap();
        let s = scan(dir.path());
        let got: Vec<(&str, &str, &str)> = s
            .findings
            .simplifications
            .iter()
            .map(|x| (x.file.as_str(), x.kind.as_str(), x.function.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("poll.py", "hand-rolled-poll", "poller"),
                ("retry.py", "hand-rolled-retry", "retryer"),
                ("swallow.py", "silent-swallow", "fetcher"),
            ]
        );
        // A URL-literal poll used no SDK shape, so doctor has no route key to
        // propose for it.
        assert!(
            s.findings
                .simplifications
                .iter()
                .all(|x| x.sdk_polls.is_empty()),
            "{:?}",
            s.findings.simplifications
        );
        // Anchor line: the poll sighting points at the `while` (line 7), the
        // construct to delete — not the sleep inside it (line 12).
        let poll = &s.findings.simplifications[0];
        assert_eq!(poll.line, 7);
        assert_eq!(poll.targets, vec!["api.tavily.com".to_owned()]);
    }

    /// #107.1: a sighting carries the loop's OWN cadence when the source
    /// states it — the sleep literal and a single whole-second deadline
    /// literal the function names. Anything not statically certain (a
    /// computed backoff, a fractional sleep, two disagreeing deadline
    /// literals) stays `None`, so the proposal falls back to its documented
    /// default instead of inventing a number.
    #[test]
    fn poll_sightings_carry_the_loops_own_interval_and_deadline() {
        if !python3_present() {
            eprintln!("skip: python3 not available");
            return;
        }
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("literal.py"),
            r#"import time
import urllib.request

API = "https://api.tavily.com/research"

def poller(request_id, timeout_s: int = 900):
    while True:
        with urllib.request.urlopen(API) as r:
            data = r.read().decode()
        if data == "completed":
            return data
        time.sleep(10)
"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("computed.py"),
            r#"import time
import urllib.request

API = "https://api.tavily.com/research"

def poller(backoff):
    while True:
        with urllib.request.urlopen(API) as r:
            data = r.read().decode()
        if data == "completed":
            return data
        time.sleep(backoff)
"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("fractional.py"),
            r#"import time
import urllib.request

API = "https://api.tavily.com/research"

def poller(timeout_s: int = 60, deadline_s: int = 120):
    while True:
        with urllib.request.urlopen(API) as r:
            data = r.read().decode()
        if data == "completed":
            return data
        time.sleep(0.5)
"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("millis.py"),
            r#"import time
import urllib.request

API = "https://api.tavily.com/research"

def poller(timeout_ms: int = 5000):
    while True:
        with urllib.request.urlopen(API) as r:
            data = r.read().decode()
        if data == "completed":
            return data
        time.sleep(2)
"#,
        )
        .unwrap();
        let s = scan(dir.path());
        let got: Vec<(&str, Option<u32>, Option<u32>)> = s
            .findings
            .simplifications
            .iter()
            .map(|x| (x.file.as_str(), x.interval_s, x.deadline_s))
            .collect();
        assert_eq!(
            got,
            vec![
                // A computed backoff is not a number Keel knows.
                ("computed.py", None, None),
                // `sleep(0.5)` is not whole seconds; 60 and 120 disagree.
                ("fractional.py", None, None),
                // `time.sleep(10)` + one `timeout_s: int = 900` default.
                ("literal.py", Some(10), Some(900)),
                // 5000 MILLISECONDS is not 5000 seconds — read as neither.
                ("millis.py", Some(2), None),
            ]
        );
    }

    #[test]
    fn simplification_detection_is_gated_on_target_attribution() {
        if !python3_present() {
            eprintln!("skip: python3 not available");
            return;
        }
        let dir = TempDir::new().unwrap();
        // Identical retry shape, but the function reaches NO target — the
        // conservative gate must suppress it (a sleep loop around local work is
        // none of Keel's business).
        fs::write(
            dir.path().join("local.py"),
            r"import time

def churn():
    n = 0
    while True:
        n += 1
        if n > 3:
            return n
        time.sleep(1)
",
        )
        .unwrap();
        // Narrow except (specific exception tuple, the web_search.py shape) with
        // a target in reach — NOT a silent swallow.
        fs::write(
            dir.path().join("narrow.py"),
            r#"import urllib.request
import urllib.error

API = "https://api.tavily.com/search"

def lookup():
    try:
        return urllib.request.urlopen(API)
    except (urllib.error.URLError, ValueError):
        return []
"#,
        )
        .unwrap();
        let s = scan(dir.path());
        assert!(
            s.findings.simplifications.is_empty(),
            "{:?}",
            s.findings.simplifications
        );
    }

    #[test]
    fn bare_from_import_sleep_is_still_detected() {
        // `from time import sleep` binds `sleep` -> "time" in `aliases`
        // (import_entries tracks module -> binding); a direct `sleep(1)`
        // call (an `ast.Name`, not `ast.Attribute`) resolves `name =
        // call_root(node.func) = "sleep"`, matching `is_sleep` without the
        // `time.` prefix. Issue #28 item 4: "verified correct by
        // inspection, untested" — this pins it.
        if !python3_present() {
            eprintln!("skip: python3 not available");
            return;
        }
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("retry.py"),
            r#"import httpx
from time import sleep

API = "https://api.tavily.com/search"

def retryer():
    attempts = 0
    while True:
        try:
            return httpx.get(API)
        except Exception:
            attempts += 1
            sleep(1)
"#,
        )
        .unwrap();
        let s = scan(dir.path());
        let got: Vec<(&str, &str)> = s
            .findings
            .simplifications
            .iter()
            .map(|x| (x.kind.as_str(), x.function.as_str()))
            .collect();
        assert_eq!(got, vec![("hand-rolled-retry", "retryer")]);
    }

    #[test]
    fn for_and_async_for_loops_hand_roll_retry_and_poll_directly() {
        // Coverage beyond `ast.While`: the primary loop-classification pass
        // (not the except-handler fallback) must also fire for `for` and
        // `async for` loops when the sleep/counter (or sleep/status-compare)
        // sit directly in the loop body.
        if !python3_present() {
            eprintln!("skip: python3 not available");
            return;
        }
        let dir = TempDir::new().unwrap();
        // `for`: counter + sleep directly in the loop body (not nested in
        // an except handler) — exercises the `ast.For` arm of the primary
        // classification match.
        fs::write(
            dir.path().join("for_retry.py"),
            r#"import time
import httpx

API = "https://api.tavily.com/search"

def retryer():
    attempts = 0
    for _ in range(5):
        attempts += 1
        time.sleep(1)
        try:
            return httpx.get(API)
        except Exception:
            pass
"#,
        )
        .unwrap();
        // `async for`: a status-string compare governing a `return` (the
        // poll shape) — exercises the `ast.AsyncFor` arm.
        fs::write(
            dir.path().join("async_for_poll.py"),
            r#"import asyncio
import httpx

API = "https://api.tavily.com/search"

async def poller():
    httpx.get(API)
    async for chunk in stream():
        if chunk == "done":
            return chunk
        await asyncio.sleep(1)
"#,
        )
        .unwrap();
        let s = scan(dir.path());
        let got: Vec<(&str, &str, &str)> = s
            .findings
            .simplifications
            .iter()
            .map(|x| (x.file.as_str(), x.kind.as_str(), x.function.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("async_for_poll.py", "hand-rolled-poll", "poller"),
                ("for_retry.py", "hand-rolled-retry", "retryer"),
            ]
        );
    }

    #[test]
    fn nested_hand_rolled_loops_are_each_sighted() {
        // Characterization test (issue #28 item 4): a hand-rolled loop
        // nested inside another loop is currently "double-sighted" — the
        // outer loop's own subtree walk also contains the inner loop's
        // sleep/counter, so BOTH the outer and inner `while` independently
        // satisfy the detection predicate and each produce a finding for
        // what a human would call one construct. This pins today's actual
        // behavior; it is not an assertion that double-sighting is ideal.
        if !python3_present() {
            eprintln!("skip: python3 not available");
            return;
        }
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("nested.py"),
            r#"import time
import httpx

API = "https://api.tavily.com/search"

def retryer():
    outer_attempts = 0
    while outer_attempts < 3:
        inner_attempts = 0
        while True:
            inner_attempts += 1
            try:
                return httpx.get(API)
            except Exception:
                time.sleep(1)
        outer_attempts += 1
"#,
        )
        .unwrap();
        let s = scan(dir.path());
        let got: Vec<(&str, &str)> = s
            .findings
            .simplifications
            .iter()
            .map(|x| (x.kind.as_str(), x.function.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("hand-rolled-retry", "retryer"),
                ("hand-rolled-retry", "retryer"),
            ],
            "expected today's double-sighting (outer + inner loop both flagged): {got:?}"
        );
    }

    #[test]
    fn dependency_averse_files_are_detected_and_markers_win() {
        if !python3_present() {
            eprintln!("skip: python3 not available");
            return;
        }
        let dir = TempDir::new().unwrap();
        // stdlib-only + docstring signal -> detected.
        fs::write(
            dir.path().join("risk_gate.py"),
            "\"\"\"Deterministic risk gate. stdlib only.\"\"\"\nimport json, urllib.request\n",
        )
        .unwrap();
        // stdlib-only + name signal, but explicit include marker -> NOT detected.
        fs::write(
            dir.path().join("validate.py"),
            "# keel: include\nimport json\n",
        )
        .unwrap();
        // third-party import + no signal, but explicit exclude marker -> detected.
        fs::write(dir.path().join("app.py"), "# keel: exclude\nimport httpx\n").unwrap();
        // plain file -> not detected.
        fs::write(dir.path().join("util.py"), "import json\n").unwrap();
        let s = scan(dir.path());
        let files: Vec<&str> = s
            .findings
            .dependency_averse
            .iter()
            .map(|d| d.file.as_str())
            .collect();
        assert_eq!(files, vec!["app.py", "risk_gate.py"]);
        let app = &s.findings.dependency_averse[0];
        assert_eq!(app.reason, "marker");
        let gate = &s.findings.dependency_averse[1];
        assert!(
            gate.reason.contains("risk") || gate.reason.contains("gate"),
            "{}",
            gate.reason
        );
    }

    #[test]
    fn syntax_error_file_is_skipped_not_fatal() {
        if !python3_present() {
            eprintln!("skip: python3 not available");
            return;
        }
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("broken.py"), "def (:\n").unwrap();
        fs::write(dir.path().join("ok.py"), "import requests\n").unwrap();
        let scan = scan(dir.path());
        assert_eq!(scan.files_scanned, 1, "only the parseable file counts");
        assert!(scan.findings.http_in_use);
    }

    #[test]
    #[allow(clippy::too_many_lines)] // straight-line fixture setup, one module
    // per poll shape (bound-result, inline #101, unattributed, not-a-poll).
    fn sdk_poll_calls_attribute_the_provider_target_without_a_url_literal() {
        if !python3_present() {
            eprintln!("skip: python3 not available");
            return;
        }
        let dir = TempDir::new().unwrap();
        // The ai-marketing-hub loop, verbatim shape: the client comes from a
        // factory in another module, so no alias binding exists here.
        fs::write(
            dir.path().join("render.py"),
            r#"import time
from google import genai
from .clients import get_client

def poll_video_takes(op, model_id, timeout_s=900):
    client = get_client()
    deadline = time.monotonic() + timeout_s
    while not op.done:
        if time.monotonic() > deadline:
            raise TimeoutError(f"Video render timed out ({model_id})")
        time.sleep(10)
        op = client.operations.get(op)
    return op.result
"#,
        )
        .unwrap();
        fs::write(
            dir.path().join("batches.py"),
            r#"import time
import openai

def wait(client, batch_id):
    while True:
        b = client.batches.retrieve(batch_id)
        if b.status in ("completed", "failed"):
            return b
        time.sleep(5)
"#,
        )
        .unwrap();
        // #101: the SDK poll call sits INSIDE the loop-exit test itself, so
        // no name is ever bound to its result — the governance gate must see
        // the call in the `while` test (and in a break/return-guarded `if`).
        fs::write(
            dir.path().join("inline.py"),
            r"import time
from google import genai

def wait(client, op):
    while not client.operations.get(op).done:
        time.sleep(1)
    return op

def wait_guarded(client, op):
    while True:
        if client.operations.get(op).done:
            return op
        time.sleep(1)
",
        )
        .unwrap();
        // Same loop shape, but no provider import in the module: no attribution.
        fs::write(
            dir.path().join("other.py"),
            r"import time

def wait(client, job):
    while not job.done:
        time.sleep(1)
        job = client.operations.get(job)
",
        )
        .unwrap();
        // Not a poll: `for batch_id in batch_ids` iterates DISTINCT jobs, and
        // the sleep is rate-limit pacing, not backoff — `b` is only ever
        // stored into `cache`, never read back into a loop-exit test. The SDK
        // poll call shape matches (provider imported, suffix, receiver), but
        // its result does not govern the loop's continuation, so this must
        // NOT be flagged (the false-positive class review caught).
        fs::write(
            dir.path().join("sync.py"),
            r"import time, openai

def sync_batch_metadata(client, batch_ids, cache):
    for batch_id in batch_ids:
        b = client.batches.retrieve(batch_id)
        cache[batch_id] = b
        time.sleep(0.5)
",
        )
        .unwrap();
        let s = scan(dir.path());
        let f = |module: &str, name: &str| {
            s.functions
                .iter()
                .find(|f| f.entrypoint == format!("py:{module}:{name}"))
                .unwrap_or_else(|| panic!("{module}:{name} attributed"))
        };
        assert_eq!(
            f("render", "poll_video_takes").targets,
            std::collections::BTreeSet::from(["llm:google-genai".to_owned()])
        );
        assert_eq!(
            f("batches", "wait").targets,
            std::collections::BTreeSet::from(["llm:openai".to_owned()])
        );
        assert!(f("other", "wait").targets.is_empty());
        // sync_batch_metadata DOES reach the openai target (the SDK call is
        // real) but must not be classified as a poll.
        assert!(
            f("sync", "sync_batch_metadata")
                .targets
                .contains("llm:openai")
        );
        let polls: Vec<(&str, &str)> = s
            .findings
            .simplifications
            .iter()
            .filter(|x| x.kind == "hand-rolled-poll")
            .map(|x| (x.file.as_str(), x.function.as_str()))
            .collect();
        assert_eq!(
            polls,
            vec![
                ("batches.py", "wait"),
                ("inline.py", "wait"),
                ("inline.py", "wait_guarded"),
                ("render.py", "poll_video_takes")
            ]
        );
        // The SDK shape each poll used, for doctor's route-key proposal.
        let shape = |file: &str, function: &str| {
            s.findings
                .simplifications
                .iter()
                .find(|x| x.file == file && x.function == function)
                .unwrap_or_else(|| panic!("{file}:{function} sighted"))
                .sdk_polls
                .clone()
        };
        assert_eq!(shape("render.py", "poll_video_takes"), ["operations.get"]);
        assert_eq!(shape("batches.py", "wait"), ["batches.retrieve"]);
        assert_eq!(shape("inline.py", "wait"), ["operations.get"]);
        assert!(
            !s.findings
                .simplifications
                .iter()
                .any(|x| x.file == "sync.py"),
            "iterating distinct jobs with a rate-limit sleep must not be flagged as a poll: {:?}",
            s.findings.simplifications
        );
    }
}
