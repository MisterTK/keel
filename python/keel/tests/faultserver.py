"""A tiny stdlib fault-injecting HTTP server for adapter tests.

Deterministic by construction: each incoming request pops the next directive
from a scripted, ordered queue (shared across paths), so a test controls the
exact fault sequence — e.g. ``[fail(503), ok(b"hi")]`` returns 503 then 200.
Supported faults cover the brief's cases: 5xx / 429-with-Retry-After statuses,
an abrupt connection reset, and a slow response (for client-side timeout).
Sleeps are kept tiny and are only used to trip a small client timeout.
"""

from __future__ import annotations

import socket
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any


class _QuietServer(ThreadingHTTPServer):
    """Swallows the connection errors our own fault injection provokes (resets,
    client timeouts) so test output stays pristine; anything else still surfaces."""

    daemon_threads = True

    def handle_error(self, request: Any, client_address: Any) -> None:
        if issubclass(sys.exc_info()[0] or Exception, OSError):
            return
        super().handle_error(request, client_address)


def ok(body: bytes = b"OK", headers: dict[str, str] | None = None) -> dict[str, Any]:
    return {"status": 200, "body": body, "headers": headers or {}}


def status(code: int, body: bytes = b"", headers: dict[str, str] | None = None) -> dict[str, Any]:
    return {"status": code, "body": body, "headers": headers or {}}


def fail(code: int = 503, headers: dict[str, str] | None = None) -> dict[str, Any]:
    return {"status": code, "body": b"server error", "headers": headers or {}}


def throttled(retry_after: str = "1") -> dict[str, Any]:
    return {"status": 429, "body": b"slow down", "headers": {"Retry-After": retry_after}}


def reset() -> dict[str, Any]:
    return {"reset": True}


def slow(seconds: float, then: dict[str, Any] | None = None) -> dict[str, Any]:
    d = dict(then or ok())
    d["sleep"] = seconds
    return d


class FaultServer:
    """Context-managed threaded HTTP server driving a scripted directive queue."""

    def __init__(self, script: list[dict[str, Any]]) -> None:
        self._script = list(script)
        self._i = 0
        self._lock = threading.Lock()
        self.requests: list[tuple[str, str]] = []  # (method, path) actually served
        server = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *args: Any) -> None:  # silence
                pass

            def _drain_request_body(self) -> None:
                # Read and discard any request body so a keep-alive connection is
                # left clean for the next request. Without this, an unread POST
                # body corrupts framing when the connection is reused (e.g. after
                # a 200 that httpx returns to its pool).
                length = self.headers.get("Content-Length")
                if not length:
                    return
                try:
                    remaining = int(length)
                except ValueError:
                    return
                while remaining > 0:
                    chunk = self.rfile.read(min(remaining, 65536))
                    if not chunk:
                        break
                    remaining -= len(chunk)

            def _serve(self) -> None:
                directive = server._next()
                server.requests.append((self.command, self.path))
                if directive.get("reset"):
                    try:
                        self.connection.shutdown(socket.SHUT_RDWR)
                    except OSError:
                        pass
                    self.connection.close()
                    self.close_connection = True
                    return
                self._drain_request_body()
                sleep_s = directive.get("sleep")
                if sleep_s:
                    time.sleep(sleep_s)
                body: bytes = directive.get("body", b"")
                try:
                    self.send_response(directive.get("status", 200))
                    for k, v in directive.get("headers", {}).items():
                        self.send_header(k, v)
                    self.send_header("Content-Length", str(len(body)))
                    self.end_headers()
                    if self.command != "HEAD":
                        self.wfile.write(body)
                except OSError:
                    self.close_connection = True  # client already gone (timeout)

            do_GET = _serve
            do_POST = _serve
            do_PUT = _serve
            do_DELETE = _serve
            do_HEAD = _serve
            do_OPTIONS = _serve
            do_PATCH = _serve

        self._httpd = _QuietServer(("127.0.0.1", 0), Handler)
        self._thread = threading.Thread(target=self._httpd.serve_forever, daemon=True)

    def _next(self) -> dict[str, Any]:
        with self._lock:
            if self._i < len(self._script):
                d = self._script[self._i]
                self._i += 1
                return d
        return ok(b"default")

    @property
    def port(self) -> int:
        return self._httpd.server_address[1]

    def url(self, path: str = "/") -> str:
        return f"http://127.0.0.1:{self.port}{path}"

    @property
    def served(self) -> int:
        return len(self.requests)

    def __enter__(self) -> "FaultServer":
        self._thread.start()
        return self

    def __exit__(self, *exc: Any) -> None:
        self._httpd.shutdown()
        self._httpd.server_close()
        self._thread.join(timeout=2)
