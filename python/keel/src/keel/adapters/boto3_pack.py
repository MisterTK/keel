"""The boto3 adapter pack: resilience for every AWS SDK (botocore) call,
zero code changes, through botocore's single per-operation dispatch point.

Seam: ``botocore.client.BaseClient._make_api_call`` — every generated service
client method (``S3.get_object``, ``DynamoDB.put_item``, ...) is a thin
wrapper botocore generates from the service model that calls
``self._make_api_call(operation_name, api_params)``; it is the single
dispatch point every operation on every service passes through, regardless
of protocol (``rest-json``/``rest-xml``/``query``/``json``/``ec2``).
architecture-spec §5.1 names botocore's event-hook system ("before-send" /
"after-call") as boto3's official extension point; this pack patches the
class method instead, for the same reason httpx/requests patch a class
method rather than register per-instance hooks: it covers every client
uniformly the moment botocore is imported, with no dependency on how or when
the caller constructed their client/session. It is also the same seam long
used by aws-xray-sdk, sentry-sdk, and opentelemetry-instrumentation-botocore
for exactly this kind of instrumentation.

Target: ``tool:aws.<service>`` (e.g. ``tool:aws.s3``, ``tool:aws.dynamodb``),
``<service>`` being ``client.meta.service_model.service_name`` — exactly the
string the caller passed to ``boto3.client("...")``. A raw
``*.amazonaws.com`` hostname target would fragment policy across every AWS
region/endpoint for the same logical service (S3 alone speaks from a
different host per region); ``tool:`` is the nearest kind in the frozen
target grammar (``contracts/policy.schema.json`` ``$defs.targetKey`` allows
only bare host, ``llm:``, ``tool:``, ``mcp:`` — there is no ``aws:`` kind to
mint) for "a named external capability", distinct from the unrelated
``tool:<name>`` callable-wrapping pack (``packs/tool.py``); a literal name
collision is vanishingly unlikely and, if it ever happened, would only mean
the two share one policy knob.

Idempotency (``study botocore operation metadata for idempotency``):

1. **Idempotency-token operations.** If the operation declares
   ``idempotent_members`` (the API model's ``idempotencyToken`` trait — e.g.
   EC2 ``RunInstances``' ``ClientToken``) and the caller supplied one,
   the call is safe to retry: AWS itself guarantees the re-invocation is
   deduplicated server-side.
2. **REST-protocol services** (``rest-json``/``rest-xml`` — S3, Lambda, ...)
   expose a real, meaningful wire HTTP method: GET/HEAD/OPTIONS/PUT/DELETE
   are idempotent by the same RFC 9110 judgment the HTTP packs use.
3. **query/json/ec2-protocol services** (EC2, DynamoDB, SQS, IAM, ...) send
   every operation as HTTP POST regardless of semantics, so the wire method
   carries no signal; this pack falls back to a documented, conservative
   operation-NAME-prefix heuristic (``Get``/``List``/``Describe``/... read
   verbs). A false negative (a safe read misclassified as a write) only
   under-retries — a false positive is avoided by keeping the prefix list to
   well-established AWS naming conventions. Anything else is non-idempotent:
   observed, not retried (KEEL-E014) — the safe default for a database/queue
   mutation.

Retry-loop interaction with botocore's own retries (``turn ours on only
where safe``): botocore's HTTP session already retries transient failures
internally (``Config(retries=...)``, default mode/attempts vary by botocore
version) BEFORE ``_make_api_call`` returns or raises to us — so by the time
Keel's own ``[defaults.outbound]`` retry layer (3 attempts) wraps this whole
call, a sustained throttling event can compound the two retry loops. The
frozen policy schema has no ``[defaults.tool]`` fragment this pack could use
to safely dial Keel's own retry down for every ``tool:aws.*`` target (only
``outbound``/``llm`` exist under ``defaults`` — see policy.schema.json), so
this is a documented, accepted limitation rather than something silently
"fixed" by reaching into botocore's private retry-handler wiring: the
breaker (5 failures / 15s cooldown, inherited from ``[defaults.outbound]``)
still bounds the worst case, and a caller who wants Keel to be the sole
retry authority for a service can construct that client with
``Config(retries={"max_attempts": 1})`` — a botocore-native way to cede
retry ownership, needing no Keel code. The same reasoning is why AWS
throttling errors are recognized here only via HTTP status (429/5xx, like
the HTTP packs); a small number of services signal throttling with a 4xx
status and a specific error code instead (e.g. DynamoDB's
``ProvisionedThroughputExceededException``, HTTP 400) — those are not
retried by this pack, deferring to botocore's own more nuanced handling.

Caching: never (``args_hash`` is always ``None``). Many boto3 responses
contain values ``json.dumps`` cannot serialize (``Decimal``, ``datetime``,
a streaming ``Body`` for S3 downloads); ``_wrap._json_safe`` already treats
any such value as "not safely cacheable" by returning ``None`` for the core
``payload``, so declaring caching out of scope up front is honest rather
than shipping a feature that silently no-ops for the most compelling case
(a DynamoDB ``GetItem``, whose ``Decimal`` numbers always fail
``json.dumps``).

Live objects vs. the core boundary: identical shape to ``packs/tool.py`` —
the real result dict is delivered on every live call unchanged (identity
preserved), the ORIGINAL exception re-raises on terminal failure (DX
invariant 5, ``err.response``/``err.operation_name`` intact), and only a
cache hit would return the JSON payload (dead code today, since caching
never activates — see above).
"""

from __future__ import annotations

import functools
import importlib.metadata
import importlib.util
import time
from typing import Any, Callable

from .. import _runtime
from .._wrap import _json_safe
from . import _http
from ._pack import Detection, Seam, TargetDecl

#: The import-hook trigger AND the actual patch target: botocore, not boto3.
#: boto3 is a thin sugar layer that itself imports botocore, so watching
#: "botocore" catches boto3 users and bare-botocore users alike.
MODULE = "botocore"
NAME = "boto3"

#: Versions this pack certifies via contract tests (prefix match) — botocore's
#: own version, since that is what MODULE/install() actually instruments.
_PINNED = ("1.42", "1.43")

_installed = False
_orig: dict[str, Any] = {}

_REST_PROTOCOLS = frozenset({"rest-json", "rest-xml"})
_IDEMPOTENT_HTTP_METHODS = frozenset({"GET", "HEAD", "OPTIONS", "PUT", "DELETE"})
#: Conservative, documented read-verb prefixes for query/json/ec2-protocol
#: services, where the wire HTTP method is always POST (module docs point 3).
_READ_NAME_PREFIXES = (
    "Get",
    "List",
    "Describe",
    "Head",
    "Query",
    "Scan",
    "BatchGet",
    "Lookup",
    "Check",
    "Test",
    "Validate",
    "Search",
    "Estimate",
    "Simulate",
)


# --- contract operations -----------------------------------------------------


def detect() -> Detection:
    if importlib.util.find_spec(MODULE) is None:
        return Detection(matched=False)
    try:
        version = importlib.metadata.version(MODULE)
    except importlib.metadata.PackageNotFoundError:
        version = ""
    confidence = "pinned" if _is_pinned(version) else "best_effort"
    return Detection(matched=True, name=NAME, version=version, confidence=confidence)


def seams() -> list[Seam]:
    return [
        Seam(
            patch_point="botocore.client.BaseClient._make_api_call",
            upstream_api="botocore client API: BaseClient._make_api_call(operation_name, api_params) -> dict",
            why_stable=(
                "Every generated service client method is a thin wrapper "
                "botocore builds from the service model that calls "
                "self._make_api_call(...); the single dispatch point every "
                "operation on every service passes through. "
                "architecture-spec names botocore's event hooks as the "
                "official extension point; this pack patches the class "
                "method instead so every client is covered uniformly, "
                "matching the httpx/requests seam shape (see module docs)."
            ),
        ),
    ]


def targets() -> list[TargetDecl]:
    return [
        TargetDecl(
            pattern="tool:aws.<service>",
            kind="tool",
            idempotency_rule=(
                "a caller-supplied idempotency-token member (e.g. "
                "ClientToken); else GET/HEAD/OPTIONS/PUT/DELETE on a "
                "rest-json/rest-xml service; else a read-verb operation name "
                "(Get*/List*/Describe*/...) on a query/json/ec2-protocol "
                "service; anything else is observed, not retried (KEEL-E014)"
            ),
            args_hash_rule="None always — boto3 responses are not cached by this pack (module docs)",
        )
    ]


def defaults() -> dict[str, Any]:
    """No pack-specific fragment: the frozen schema has no ``[defaults.tool]``
    table (only ``outbound``/``llm``), so ``tool:aws.*`` targets inherit
    ``[defaults.outbound]`` (module docs discuss the retry-compounding
    tradeoff this implies)."""
    return {}


# --- install / uninstall -----------------------------------------------------


def install() -> None:
    """Patch the botocore seam. Idempotent; a no-op if botocore is not
    importable."""
    global _installed
    if _installed:
        return
    try:
        import botocore.client as bc
    except ImportError:
        return
    _orig["make_api_call"] = bc.BaseClient._make_api_call
    bc.BaseClient._make_api_call = _make_api_call_wrapper(_orig["make_api_call"])  # type: ignore[method-assign]
    _installed = True


def uninstall() -> None:
    global _installed
    if not _installed:
        return
    import botocore.client as bc

    bc.BaseClient._make_api_call = _orig["make_api_call"]  # type: ignore[method-assign]
    _orig.clear()
    _installed = False


# --- judgment ----------------------------------------------------------------


def _target_for(client: Any) -> str:
    service = getattr(client.meta.service_model, "service_name", None) or "unknown"
    return f"tool:aws.{service}"


def _has_supplied_idempotency_token(operation_model: Any, api_params: Any) -> bool:
    members = getattr(operation_model, "idempotent_members", None) or ()
    if not members or not isinstance(api_params, dict):
        return False
    return any(api_params.get(m) for m in members)


def _is_idempotent_operation(operation_model: Any, api_params: Any, protocol: str | None) -> bool:
    if _has_supplied_idempotency_token(operation_model, api_params):
        return True
    if protocol in _REST_PROTOCOLS:
        http = getattr(operation_model, "http", None) or {}
        method = str(http.get("method", "")).upper() if isinstance(http, dict) else ""
        return method in _IDEMPOTENT_HTTP_METHODS
    name = getattr(operation_model, "name", "") or ""
    return name.startswith(_READ_NAME_PREFIXES)


def _judge(client: Any, operation_name: str, api_params: Any) -> tuple[str, bool]:
    target = _target_for(client)
    service_model = client.meta.service_model
    try:
        operation_model = service_model.operation_model(operation_name)
    except Exception:
        return target, False  # unknown operation (future API surface): conservative
    idempotent = _is_idempotent_operation(operation_model, api_params, getattr(service_model, "protocol", None))
    return target, idempotent


def _classify(err: BaseException) -> str:
    import botocore.exceptions as be

    if isinstance(err, (be.ConnectTimeoutError, be.ReadTimeoutError)):
        return "timeout"
    if isinstance(err, be.ConnectionError):
        return "conn"
    return "other"


def _client_error_response(err: BaseException) -> dict[str, Any] | None:
    response = getattr(err, "response", None)
    return response if isinstance(response, dict) else None


def _client_error_status(err: BaseException) -> int | None:
    response = _client_error_response(err)
    meta = response.get("ResponseMetadata") if response else None
    status = meta.get("HTTPStatusCode") if isinstance(meta, dict) else None
    return status if isinstance(status, int) else None


def _client_error_retry_after(err: BaseException) -> str | None:
    response = _client_error_response(err)
    meta = response.get("ResponseMetadata") if response else None
    headers = meta.get("HTTPHeaders") if isinstance(meta, dict) else None
    if not isinstance(headers, dict):
        return None
    return headers.get("retry-after") or headers.get("Retry-After")


# --- seam ---------------------------------------------------------------------


def _run(client: Any, orig: Callable[..., Any], operation_name: str, api_params: Any) -> Any:
    backend = _runtime.get_backend()
    if backend is None:
        return orig(client, operation_name, api_params)  # disabled / uninstalled: transparent
    discovery = _runtime.get_discovery()
    target, idempotent = _judge(client, operation_name, api_params)
    env = {
        "v": _http.ENVELOPE_VERSION,
        "target": target,
        "op": f"{operation_name} {target}",
        "idempotent": idempotent,
        "args_hash": None,  # never cached (module docs)
    }
    live: dict[str, Any] = {"result": None, "have": False, "exc": None}

    def effect(_attempt: int) -> dict[str, Any]:
        try:
            # botocore's default HTTP session dispatches through urllib3; mark
            # this attempt so the urllib3 pack passes its inner call straight
            # through instead of judging (and retrying) it a second time.
            result = _http.run_owned(lambda: orig(client, operation_name, api_params))
        except Exception as err:  # not BaseException: let exit/interrupt fly
            live["exc"] = err
            status = _client_error_status(err)
            if status is not None and _http.is_transient_status(status):
                return _http.transient_error(status, _client_error_retry_after(err))
            return _http.thrown_error(err, _classify(err))
        live["have"] = True
        live["exc"] = None
        live["result"] = result
        return {"status": "ok", "payload": _json_safe(result)}

    started = time.perf_counter()
    outcome = backend.execute(env, effect)
    latency_ms = round((time.perf_counter() - started) * 1000)
    if discovery is not None:
        discovery.record(target, outcome, latency_ms)

    action, value = _http.deliver(
        outcome,
        ok_response=live["result"],
        transient_response=None,  # every boto3 failure is a raised exception, never a "bad" return
        exc=live["exc"],
        rebuild=lambda payload: payload,  # dead code: args_hash is always None, so never a cache hit
    )
    if action == "return":
        return value
    raise value


def _make_api_call_wrapper(orig: Callable[..., Any]) -> Callable[..., Any]:
    @functools.wraps(orig)
    def _make_api_call(self: Any, operation_name: str, api_params: Any) -> Any:
        return _run(self, orig, operation_name, api_params)

    _make_api_call.__keel_wrapped__ = True  # type: ignore[attr-defined]
    return _make_api_call


def _is_pinned(version: str) -> bool:
    return any(version == p or version.startswith(p + ".") for p in _PINNED)


__all__ = ["MODULE", "NAME", "detect", "seams", "targets", "defaults", "install", "uninstall"]
