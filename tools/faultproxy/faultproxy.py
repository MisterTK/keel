#!/usr/bin/env python3
"""faultproxy — a scriptable, deterministic fault-injecting HTTP server/proxy.

Zero dependencies (Python stdlib only): every Keel team already requires
python3, so a single stdlib file is the lowest-friction way to give the demos
and adapter tests a reusable fault source with no build step. It is used by at
least the flaky-python, node-service, and agent demos.

# What it does

Given a JSON *scenario file*, faultproxy serves each request path from its own
ordered sequence of directives. The Nth request to a path gets the Nth
directive — so a test or demo controls the exact fault sequence deterministically
(e.g. `/flaky` → [503, 200] returns 503 then 200). This is the per-path
generalization of the tests' single-queue `faultserver.py`; existing adapter
tests MAY migrate to it but are not required to.

# Scenario format (JSON)

    {
      "upstream": "http://127.0.0.1:9000",   // optional reverse-proxy target
      "default": { "status": 200, "body": "ok" }, // optional when a seq is spent
      "paths": {
        "/flaky": [ {"status": 503}, {"status": 200, "body": "late"} ],
        "/v1/chat/completions": [
          {"status": 429, "headers": {"Retry-After": "0"}, "repeat": 2},
          {"status": 200, "body": "{\"reply\":\"hi\"}",
           "headers": {"content-type": "application/json"}}
        ],
        "*": [ {"forward": true} ]           // wildcard fallback path
      }
    }

Directive fields (all optional): `status` (int, default 200), `body` (str),
`headers` (obj), `delay_ms` (int), `reset` (true → drop the connection),
`repeat` (int → serve this directive N times before advancing), `forward`
(true → reverse-proxy this request to `upstream`).

Sequencing when a path's list is spent, in order: `default` → forward to
`upstream` (if set) → a plain `200 faultproxy: no directive`.

# Control endpoints (not part of the scenario)

    GET  /__faultproxy__/log     → JSON [{method,path,status}, ...] served so far
    POST /__faultproxy__/reset   → reset all per-path cursors and the log

# CLI

    faultproxy.py --scenario s.json [--host 127.0.0.1] [--port 0] [--port-file f]

`--port 0` (default) binds an ephemeral port; the chosen port is printed to
stdout as `PORT=<n>` (first line) and, if `--port-file` is given, written there —
so a `run.sh` can capture it without racing the banner.
"""

from __future__ import annotations

import argparse
import json
import socket
import sys
import threading
import time
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Any
from urllib.error import HTTPError, URLError

_CONTROL_PREFIX = "/__faultproxy__/"


class Scenario:
    """The pure, network-free sequencing core (unit-tested directly).

    `next_directive(path)` returns the resolved directive dict for the next
    request to `path`, advancing that path's cursor. Deterministic given the
    per-path request order. A `repeat` count serves one directive N times before
    the cursor advances. When a path's sequence is spent, falls back to the
    scenario `default`, else a `forward` directive if an upstream is configured,
    else a plain 200.
    """

    def __init__(self, spec: dict[str, Any]) -> None:
        self.upstream: str | None = spec.get("upstream")
        self.default: dict[str, Any] | None = spec.get("default")
        raw_paths = spec.get("paths", {})
        if not isinstance(raw_paths, dict):
            raise ValueError("scenario 'paths' must be an object of path -> [directive]")
        # Per-path directive lists and cursors. Cursor is (index, uses_of_current).
        self._paths: dict[str, list[dict[str, Any]]] = {}
        for path, directives in raw_paths.items():
            if not isinstance(directives, list):
                raise ValueError(f"scenario path {path!r} must map to a list of directives")
            self._paths[path] = [dict(d) for d in directives]
        self._cursor: dict[str, tuple[int, int]] = {}
        self._lock = threading.Lock()

    def _spent_fallback(self) -> dict[str, Any]:
        if self.default is not None:
            return dict(self.default)
        if self.upstream is not None:
            return {"forward": True}
        return {"status": 200, "body": "faultproxy: no directive"}

    def next_directive(self, path: str) -> dict[str, Any]:
        """The resolved directive for the next request to `path`; advances the
        cursor. Exact path match wins; else the `"*"` wildcard list; else the
        spent fallback."""
        with self._lock:
            key = path if path in self._paths else ("*" if "*" in self._paths else None)
            if key is None:
                return self._spent_fallback()
            directives = self._paths[key]
            index, used = self._cursor.get(key, (0, 0))
            if index >= len(directives):
                return self._spent_fallback()
            directive = directives[index]
            repeat = max(1, int(directive.get("repeat", 1)))
            used += 1
            if used >= repeat:
                self._cursor[key] = (index + 1, 0)
            else:
                self._cursor[key] = (index, used)
            return dict(directive)

    def reset(self) -> None:
        with self._lock:
            self._cursor.clear()


def _load_scenario(path: str) -> Scenario:
    with open(path, "r", encoding="utf-8") as f:
        return Scenario(json.load(f))


class _QuietServer(ThreadingHTTPServer):
    """Swallows the OS errors our own resets/timeouts provoke so output stays
    clean; anything unexpected still surfaces."""

    daemon_threads = True

    def handle_error(self, request: Any, client_address: Any) -> None:
        if issubclass(sys.exc_info()[0] or Exception, OSError):
            return
        super().handle_error(request, client_address)


class FaultProxy:
    """A context-managed threaded server driving a `Scenario`."""

    def __init__(self, scenario: Scenario, host: str = "127.0.0.1", port: int = 0) -> None:
        self.scenario = scenario
        self.log: list[dict[str, Any]] = []
        self._log_lock = threading.Lock()
        proxy = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *args: Any) -> None:  # silence access logs
                pass

            def _path_only(self) -> str:
                return self.path.split("?", 1)[0]

            def _read_body(self) -> bytes:
                length = self.headers.get("Content-Length")
                if not length:
                    return b""
                try:
                    return self.rfile.read(int(length))
                except (ValueError, OSError):
                    return b""

            def _handle_control(self, path: str) -> bool:
                if not path.startswith(_CONTROL_PREFIX):
                    return False
                if path == _CONTROL_PREFIX + "log" and self.command == "GET":
                    self._read_body()
                    proxy._send(self, 200, proxy._log_json(), {"content-type": "application/json"})
                elif path == _CONTROL_PREFIX + "reset" and self.command == "POST":
                    self._read_body()
                    proxy.scenario.reset()
                    with proxy._log_lock:
                        proxy.log.clear()
                    proxy._send(self, 200, b"reset", {})
                else:
                    self._read_body()
                    proxy._send(self, 404, b"faultproxy: unknown control endpoint", {})
                return True

            def _serve(self) -> None:
                path = self._path_only()
                if self._handle_control(path):
                    return
                body_in = self._read_body()
                directive = proxy.scenario.next_directive(path)
                if directive.get("reset"):
                    proxy._record(self.command, path, "reset")
                    proxy._drop(self)
                    return
                delay_ms = int(directive.get("delay_ms", 0))
                if delay_ms:
                    time.sleep(delay_ms / 1000.0)
                if directive.get("forward"):
                    proxy._forward(self, path, body_in)
                    return
                status = int(directive.get("status", 200))
                body = str(directive.get("body", "")).encode("utf-8")
                headers = {str(k): str(v) for k, v in directive.get("headers", {}).items()}
                proxy._record(self.command, path, status)
                proxy._send(self, status, body, headers)

            do_GET = _serve
            do_POST = _serve
            do_PUT = _serve
            do_DELETE = _serve
            do_HEAD = _serve
            do_PATCH = _serve
            do_OPTIONS = _serve

        self._httpd = _QuietServer((host, port), Handler)
        self._thread = threading.Thread(target=self._httpd.serve_forever, daemon=True)

    # --- response helpers (instance methods so the handler can reach state) ---

    def _record(self, method: str, path: str, status: Any) -> None:
        with self._log_lock:
            self.log.append({"method": method, "path": path, "status": status})

    def _log_json(self) -> bytes:
        with self._log_lock:
            return json.dumps(list(self.log)).encode("utf-8")

    @staticmethod
    def _send(handler: BaseHTTPRequestHandler, status: int, body: bytes, headers: dict) -> None:
        try:
            handler.send_response(status)
            for k, v in headers.items():
                handler.send_header(k, v)
            handler.send_header("Content-Length", str(len(body)))
            handler.end_headers()
            if handler.command != "HEAD":
                handler.wfile.write(body)
        except OSError:
            handler.close_connection = True  # client already gone

    @staticmethod
    def _drop(handler: BaseHTTPRequestHandler) -> None:
        try:
            handler.connection.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass
        handler.connection.close()
        handler.close_connection = True

    def _forward(self, handler: BaseHTTPRequestHandler, path: str, body_in: bytes) -> None:
        upstream = self.scenario.upstream
        if not upstream:
            self._record(handler.command, path, 502)
            self._send(handler, 502, b"faultproxy: forward with no upstream", {})
            return
        url = upstream.rstrip("/") + path
        req = urllib.request.Request(url, data=body_in or None, method=handler.command)
        for k, v in handler.headers.items():
            if k.lower() not in ("host", "content-length"):
                req.add_header(k, v)
        try:
            with urllib.request.urlopen(req, timeout=10) as resp:  # noqa: S310 (loopback demo)
                payload = resp.read()
                headers = {k: v for k, v in resp.headers.items() if k.lower() != "content-length"}
                self._record(handler.command, path, resp.status)
                self._send(handler, resp.status, payload, headers)
        except HTTPError as e:
            payload = e.read()
            self._record(handler.command, path, e.code)
            self._send(handler, e.code, payload, {})
        except URLError as e:
            self._record(handler.command, path, 502)
            self._send(handler, 502, f"faultproxy: upstream error: {e}".encode("utf-8"), {})

    @property
    def port(self) -> int:
        return self._httpd.server_address[1]

    def url(self, path: str = "/") -> str:
        return f"http://127.0.0.1:{self.port}{path}"

    def __enter__(self) -> "FaultProxy":
        self._thread.start()
        return self

    def __exit__(self, *exc: Any) -> None:
        self._httpd.shutdown()
        self._httpd.server_close()
        self._thread.join(timeout=2)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Scriptable fault-injecting HTTP proxy.")
    parser.add_argument("--scenario", required=True, help="path to the JSON scenario file")
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=0, help="0 = pick an ephemeral port")
    parser.add_argument("--port-file", default=None, help="write the chosen port here")
    args = parser.parse_args(argv)

    scenario = _load_scenario(args.scenario)
    proxy = FaultProxy(scenario, host=args.host, port=args.port)
    with proxy:
        port = proxy.port
        # First stdout line is machine-readable for run.sh capture.
        sys.stdout.write(f"PORT={port}\n")
        sys.stdout.flush()
        if args.port_file:
            with open(args.port_file, "w", encoding="utf-8") as f:
                f.write(str(port))
        sys.stderr.write(f"faultproxy ▸ listening on http://{args.host}:{port} — Ctrl-C to stop\n")
        sys.stderr.flush()
        try:
            while True:
                time.sleep(3600)
        except KeyboardInterrupt:
            sys.stderr.write("faultproxy ▸ stopping\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
