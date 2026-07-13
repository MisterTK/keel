"""boto3 pack tests against a structural fake of botocore (boto3/botocore are
not installed in this environment and must never become a repo dependency —
see CLAUDE.md). The fake mirrors just the shapes ``boto3_pack`` actually
touches: ``BaseClient._make_api_call``, ``client.meta.service_model``
(``operation_model()``/``protocol``/``service_name``), and the
``botocore.exceptions`` hierarchy — enough to drive the real production code
path (install/uninstall, judgment, retry, delivery) fully offline.

The design was verified against the REAL boto3/botocore (via
``botocore.stub.Stubber``) in a throwaway venv during development; this fake
reproduces those same observed shapes."""

from __future__ import annotations

import importlib.machinery
import sqlite3
import sys
import types
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory
from typing import Any

from keel import _runtime
from keel._backend import load_backend
from keel._defaults import level0_defaults
from keel._discovery import Discovery
from keel._errors import KeelError


# --- the structural fake ------------------------------------------------------


class _FakeBotoCoreError(Exception):
    pass


class _FakeConnectionError(_FakeBotoCoreError):
    pass


class _FakeEndpointConnectionError(_FakeConnectionError):
    pass


class _FakeConnectTimeoutError(_FakeConnectionError):
    pass


class _FakeReadTimeoutError(_FakeBotoCoreError):
    pass


class _FakeClientError(Exception):
    """Mirrors botocore.exceptions.ClientError: ``.response`` carries
    ``Error``/``ResponseMetadata``, exactly what ``boto3_pack`` reads."""

    def __init__(self, response: dict[str, Any], operation_name: str) -> None:
        self.response = response
        self.operation_name = operation_name
        super().__init__(f"{operation_name}: {response.get('Error')}")


def _client_error(status: int, code: str = "InternalError", retry_after: str | None = None) -> _FakeClientError:
    headers = {"retry-after": retry_after} if retry_after else {}
    response = {
        "Error": {"Code": code, "Message": "boom"},
        "ResponseMetadata": {"HTTPStatusCode": status, "HTTPHeaders": headers},
    }
    return _FakeClientError(response, "FakeOperation")


class _FakeOperationModel:
    def __init__(self, name: str, http_method: str, idempotent_members: tuple[str, ...] = ()) -> None:
        self.name = name
        self.http = {"method": http_method}
        self.idempotent_members = list(idempotent_members)


class _FakeServiceModel:
    def __init__(self, service_name: str, protocol: str, operations: dict[str, _FakeOperationModel]) -> None:
        self.service_name = service_name
        self.protocol = protocol
        self._operations = operations

    def operation_model(self, name: str) -> _FakeOperationModel:
        return self._operations[name]


class _FakeMeta:
    def __init__(self, service_model: _FakeServiceModel) -> None:
        self.service_model = service_model


class _FakeBaseClient:
    """The seam target: ``_make_api_call`` is exactly what ``boto3_pack``
    patches on the real ``botocore.client.BaseClient``."""

    def __init__(self, service_model: _FakeServiceModel, script: list[Any]) -> None:
        self.meta = _FakeMeta(service_model)
        self._script = list(script)
        self.calls: list[tuple[str, Any]] = []

    def _make_api_call(self, operation_name: str, api_params: Any) -> Any:
        self.calls.append((operation_name, api_params))
        directive = self._script.pop(0) if self._script else {}
        if isinstance(directive, BaseException):
            raise directive
        return directive


def _install_fake_botocore() -> types.ModuleType:
    """Register a fake ``botocore``/``botocore.client``/``botocore.exceptions``
    tree in ``sys.modules`` (removed by the caller via ``_uninstall_fake``)."""
    root = types.ModuleType("botocore")
    root.__spec__ = importlib.machinery.ModuleSpec("botocore", loader=None)
    root.__path__ = []  # mark as a package so submodule imports resolve

    client_mod = types.ModuleType("botocore.client")
    client_mod.BaseClient = _FakeBaseClient

    exceptions_mod = types.ModuleType("botocore.exceptions")
    exceptions_mod.BotoCoreError = _FakeBotoCoreError
    exceptions_mod.ConnectionError = _FakeConnectionError
    exceptions_mod.EndpointConnectionError = _FakeEndpointConnectionError
    exceptions_mod.ConnectTimeoutError = _FakeConnectTimeoutError
    exceptions_mod.ReadTimeoutError = _FakeReadTimeoutError
    exceptions_mod.ClientError = _FakeClientError

    root.client = client_mod
    root.exceptions = exceptions_mod
    sys.modules["botocore"] = root
    sys.modules["botocore.client"] = client_mod
    sys.modules["botocore.exceptions"] = exceptions_mod
    return root


def _uninstall_fake_botocore() -> None:
    for name in ("botocore.exceptions", "botocore.client", "botocore"):
        sys.modules.pop(name, None)


# --- tests ---------------------------------------------------------------------


class Boto3TestBase(unittest.TestCase):
    def setUp(self) -> None:
        _install_fake_botocore()
        self.addCleanup(_uninstall_fake_botocore)
        self._tmp = TemporaryDirectory()
        self.cwd = Path(self._tmp.name)
        self.backend = load_backend("stub")
        self.backend.configure(level0_defaults())
        self.discovery = Discovery(self.cwd)
        _runtime.set_runtime(self.backend, self.discovery)
        from keel.adapters import boto3_pack

        self.boto3_pack = boto3_pack
        boto3_pack.install()
        self.addCleanup(boto3_pack.uninstall)

    def tearDown(self) -> None:
        _runtime.clear_runtime()
        self.discovery.close()
        self._tmp.cleanup()

    def s3(self, script: list[Any]) -> _FakeBaseClient:
        model = _FakeServiceModel(
            "s3",
            "rest-xml",
            {
                "GetObject": _FakeOperationModel("GetObject", "GET"),
                "PutObject": _FakeOperationModel("PutObject", "PUT"),
            },
        )
        return _FakeBaseClient(model, script)

    def dynamodb(self, script: list[Any]) -> _FakeBaseClient:
        model = _FakeServiceModel(
            "dynamodb",
            "json",
            {
                "GetItem": _FakeOperationModel("GetItem", "POST"),
                "PutItem": _FakeOperationModel("PutItem", "POST"),
            },
        )
        return _FakeBaseClient(model, script)

    def ec2(self, script: list[Any]) -> _FakeBaseClient:
        model = _FakeServiceModel(
            "ec2",
            "ec2",
            {
                "RunInstances": _FakeOperationModel("RunInstances", "POST", idempotent_members=("ClientToken",)),
                "DescribeInstances": _FakeOperationModel("DescribeInstances", "POST"),
            },
        )
        return _FakeBaseClient(model, script)

    def rows(self) -> dict[str, sqlite3.Row]:
        self.discovery.close()
        conn = sqlite3.connect(self.discovery.db_path)
        conn.row_factory = sqlite3.Row
        try:
            return {r["target"]: r for r in conn.execute("SELECT * FROM discovery")}
        finally:
            conn.close()


class ContractTest(Boto3TestBase):
    def test_detect_reports_boto3_present(self) -> None:
        d = self.boto3_pack.detect()
        self.assertTrue(d.matched)
        self.assertEqual(d.name, "boto3")

    def test_seams_and_targets_and_defaults(self) -> None:
        seams = self.boto3_pack.seams()
        self.assertEqual(seams[0].patch_point, "botocore.client.BaseClient._make_api_call")
        targets = self.boto3_pack.targets()
        self.assertEqual(targets[0].pattern, "tool:aws.<service>")
        self.assertEqual(targets[0].kind, "tool")
        self.assertEqual(self.boto3_pack.defaults(), {})

    def test_install_is_idempotent_and_uninstall_restores_identity(self) -> None:
        import botocore.client as bc

        pristine_id = id(bc.BaseClient._make_api_call.__wrapped__)  # functools.wraps preserves __wrapped__
        self.boto3_pack.install()  # second call: no-op
        self.assertTrue(getattr(bc.BaseClient._make_api_call, "__keel_wrapped__", False))
        self.boto3_pack.uninstall()
        self.assertFalse(getattr(bc.BaseClient._make_api_call, "__keel_wrapped__", False))
        self.assertEqual(id(bc.BaseClient._make_api_call), pristine_id)
        self.boto3_pack.install()  # tests' own addCleanup expects it installed


class TransparencyAndTargetTest(Boto3TestBase):
    def test_success_returns_the_live_result_unchanged(self) -> None:
        client = self.s3([{"Body": "live-object", "ContentLength": 4}])
        result = client._make_api_call("GetObject", {"Bucket": "b", "Key": "k"})
        self.assertEqual(result, {"Body": "live-object", "ContentLength": 4})
        self.assertEqual(client.calls, [("GetObject", {"Bucket": "b", "Key": "k"})])

    def test_target_is_service_scoped_not_host_scoped(self) -> None:
        client = self.dynamodb([{"Item": {}}])
        client._make_api_call("GetItem", {"TableName": "t"})
        self.assertIn("tool:aws.dynamodb", self.rows())


class IdempotencyJudgmentTest(Boto3TestBase):
    def test_rest_get_is_idempotent_and_retried(self) -> None:
        client = self.s3([_client_error(500), {"Body": "recovered"}])
        out = client._make_api_call("GetObject", {"Bucket": "b", "Key": "k"})
        self.assertEqual(out, {"Body": "recovered"})
        row = self.rows()["tool:aws.s3"]
        self.assertEqual(row["attempts"], 2)
        self.assertEqual(row["retries"], 1)

    def test_rest_put_is_idempotent(self) -> None:
        client = self.s3([_client_error(500), {"ETag": "x"}])
        out = client._make_api_call("PutObject", {"Bucket": "b", "Key": "k", "Body": b"x"})
        self.assertEqual(out, {"ETag": "x"})
        self.assertEqual(self.rows()["tool:aws.s3"]["attempts"], 2)

    def test_json_protocol_write_is_not_idempotent(self) -> None:
        client = self.dynamodb([_client_error(500), {"unreached": True}])
        with self.assertRaises(_FakeClientError) as ctx:
            client._make_api_call("PutItem", {"TableName": "t", "Item": {}})
        self.assertEqual(ctx.exception.keel_outcome["error"]["code"], "KEEL-E014")
        self.assertEqual(ctx.exception.keel_outcome["attempts"], 1)
        self.assertEqual(len(client.calls), 1, "NOT retried")

    def test_json_protocol_read_uses_name_heuristic_and_is_retried(self) -> None:
        client = self.dynamodb([_client_error(500), {"Item": {"id": {"S": "1"}}}])
        out = client._make_api_call("GetItem", {"TableName": "t", "Key": {}})
        self.assertEqual(out, {"Item": {"id": {"S": "1"}}})
        self.assertEqual(len(client.calls), 2, "GetItem read-verb heuristic makes it idempotent")

    def test_idempotency_token_supplied_makes_a_write_idempotent(self) -> None:
        client = self.ec2([_client_error(500), {"Instances": []}])
        out = client._make_api_call("RunInstances", {"ClientToken": "abc", "MinCount": 1, "MaxCount": 1})
        self.assertEqual(out, {"Instances": []})
        self.assertEqual(len(client.calls), 2)

    def test_idempotency_token_operation_without_a_token_is_not_idempotent(self) -> None:
        client = self.ec2([_client_error(500), {"unreached": True}])
        with self.assertRaises(_FakeClientError):
            client._make_api_call("RunInstances", {"MinCount": 1, "MaxCount": 1})
        self.assertEqual(len(client.calls), 1)

    def test_ec2_protocol_read_verb_heuristic(self) -> None:
        client = self.ec2([_client_error(500), {"Reservations": []}])
        out = client._make_api_call("DescribeInstances", {})
        self.assertEqual(out, {"Reservations": []})
        self.assertEqual(len(client.calls), 2)


class ErrorClassificationTest(Boto3TestBase):
    def test_non_transient_client_error_is_not_retried(self) -> None:
        client = self.s3([_client_error(404, code="NoSuchKey"), {"unreached": True}])
        with self.assertRaises(_FakeClientError) as ctx:
            client._make_api_call("GetObject", {"Bucket": "b", "Key": "missing"})
        self.assertEqual(len(client.calls), 1, "a real 404 is the program's business, not retried")
        self.assertIs(ctx.exception.keel_outcome["error"]["original"], ctx.exception)

    def test_connect_timeout_is_classified_timeout_and_retried(self) -> None:
        client = self.s3([_FakeConnectTimeoutError("slow"), {"Body": "ok"}])
        out = client._make_api_call("GetObject", {"Bucket": "b", "Key": "k"})
        self.assertEqual(out, {"Body": "ok"})
        self.assertEqual(len(client.calls), 2)

    def test_endpoint_connection_error_is_classified_conn_and_retried(self) -> None:
        client = self.s3([_FakeEndpointConnectionError("refused"), {"Body": "ok"}])
        out = client._make_api_call("GetObject", {"Bucket": "b", "Key": "k"})
        self.assertEqual(out, {"Body": "ok"})
        self.assertEqual(len(client.calls), 2)

    def test_retry_after_header_governs_backoff(self) -> None:
        client = self.s3([_client_error(429, code="SlowDown", retry_after="1"), {"Body": "ok"}])
        out = client._make_api_call("GetObject", {"Bucket": "b", "Key": "k"})
        self.assertEqual(out, {"Body": "ok"})
        row = self.rows()["tool:aws.s3"]
        self.assertEqual(row["attempts"], 2)

    def test_original_exception_reraised_unchanged_on_exhaustion(self) -> None:
        self.backend.configure(
            {**level0_defaults(), "target": {"tool:aws.s3": {"retry": {"attempts": 1, "on": ["5xx"], "schedule": "fixed(1ms)"}}}}
        )
        client = self.s3([_client_error(500), {"unreached": True}])
        with self.assertRaises(_FakeClientError) as ctx:
            client._make_api_call("GetObject", {"Bucket": "b", "Key": "k"})
        exc = ctx.exception
        self.assertNotIsInstance(exc, KeelError)  # the real botocore error, not a synthesized one
        self.assertIs(exc.keel_outcome["error"]["original"], exc)


class NoCachingTest(Boto3TestBase):
    def test_judge_returns_target_and_idempotency_only_no_args_hash_concept(self) -> None:
        client = self.s3([])
        target, idempotent = self.boto3_pack._judge(client, "GetObject", {"Bucket": "b", "Key": "k"})
        self.assertEqual(target, "tool:aws.s3")
        self.assertTrue(idempotent)

    def test_never_served_from_cache_even_for_an_idempotent_read(self) -> None:
        client = self.s3([{"Body": "x"}, {"Body": "y"}])
        self.backend.configure({**level0_defaults(), "target": {"tool:aws.s3": {"cache": {"ttl": "10s"}}}})
        first = client._make_api_call("GetObject", {"Bucket": "b", "Key": "k"})
        second = client._make_api_call("GetObject", {"Bucket": "b", "Key": "k"})
        self.assertEqual(first, {"Body": "x"})
        self.assertEqual(second, {"Body": "y"}, "never served from cache, even with a cache ttl configured")
        self.assertEqual(len(client.calls), 2)


if __name__ == "__main__":
    unittest.main()
