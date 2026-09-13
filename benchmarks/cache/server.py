#!/usr/bin/env python3
"""Disk-backed HTTPS cache fixture with latency and transfer counters.

This is intentionally a benchmark fixture, not a production cache server. It
implements the same immutable HTTP v1 routes consumed by Octa, while keeping
large blobs on disk and exposing out-of-band metrics to the local coordinator.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import socket
import ssl
import tempfile
import threading
import time
from http import HTTPStatus
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any
from urllib.parse import parse_qs, urlsplit


PROTOCOL_HEADER = "X-Octa-Cache-Protocol"
PROTOCOL_VERSION = "1"
JSON_CONTENT_TYPE = "application/vnd.octa.cache.v1+json"
BLOB_CONTENT_TYPE = "application/vnd.octa.cache.blob"
SAFE_SEGMENT = re.compile(r"^[a-zA-Z0-9_.-]+$")


class Metrics:
    """Thread-safe counters observed outside measured cache requests."""

    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._values: dict[str, Any] = {
            "requests": 0,
            "metadata_bytes_uploaded": 0,
            "metadata_bytes_downloaded": 0,
            "blob_bytes_uploaded": 0,
            "blob_bytes_downloaded": 0,
            "routes": {},
        }

    def request(self, route: str) -> None:
        with self._lock:
            self._values["requests"] += 1
            routes = self._values["routes"]
            routes[route] = routes.get(route, 0) + 1

    def add(self, field: str, count: int) -> None:
        with self._lock:
            self._values[field] += count

    def snapshot(self) -> dict[str, Any]:
        with self._lock:
            return json.loads(json.dumps(self._values))


class FileStore:
    """Create-if-absent object storage used by the HTTP request handlers."""

    def __init__(self, root: Path) -> None:
        self.root = root
        self.actions = root / "actions"
        self.blobs = root / "blobs"
        self.actions.mkdir(parents=True, exist_ok=True)
        self.blobs.mkdir(parents=True, exist_ok=True)

    @staticmethod
    def _segment(value: str) -> str:
        if not SAFE_SEGMENT.fullmatch(value):
            raise ValueError("unsafe cache route segment")
        return value

    def action_path(self, namespace: str, parts: list[str]) -> Path:
        if len(parts) != 3:
            raise ValueError("invalid action route")
        safe = [self._segment(part) for part in parts]
        namespace_key = hashlib.sha256(namespace.encode("utf-8")).hexdigest()
        return self.actions / namespace_key / ("__".join(safe) + ".json")

    def blob_path(self, parts: list[str]) -> Path:
        if len(parts) != 6:
            raise ValueError("invalid blob route")
        return self.blobs / "__".join(self._segment(part) for part in parts)

    def publish_bytes(self, destination: Path, body: bytes) -> HTTPStatus:
        destination.parent.mkdir(parents=True, exist_ok=True)
        if destination.exists():
            return HTTPStatus.NO_CONTENT if destination.read_bytes() == body else HTTPStatus.CONFLICT
        temporary = destination.parent / f".{destination.name}.{os.getpid()}.{threading.get_ident()}.tmp"
        temporary.write_bytes(body)
        try:
            os.link(temporary, destination)
            return HTTPStatus.CREATED
        except FileExistsError:
            return HTTPStatus.NO_CONTENT if destination.read_bytes() == body else HTTPStatus.CONFLICT
        finally:
            temporary.unlink(missing_ok=True)

    def publish_stream(self, destination: Path, chunks: Any, length: int) -> HTTPStatus:
        destination.parent.mkdir(parents=True, exist_ok=True)
        temporary = destination.parent / f".{destination.name}.{os.getpid()}.{threading.get_ident()}.tmp"
        written = 0
        with temporary.open("wb") as sink:
            for chunk in chunks:
                sink.write(chunk)
                written += len(chunk)
                if written > length:
                    raise ValueError("request body exceeds encoded blob length")
            if written != length:
                raise EOFError("request body ended before encoded blob length")
            sink.flush()
            os.fsync(sink.fileno())
        try:
            if destination.exists():
                return HTTPStatus.NO_CONTENT if _same_file_bytes(destination, temporary) else HTTPStatus.CONFLICT
            try:
                os.link(temporary, destination)
                return HTTPStatus.CREATED
            except FileExistsError:
                return HTTPStatus.NO_CONTENT if _same_file_bytes(destination, temporary) else HTTPStatus.CONFLICT
        finally:
            temporary.unlink(missing_ok=True)


def _same_file_bytes(left: Path, right: Path) -> bool:
    if left.stat().st_size != right.stat().st_size:
        return False
    with left.open("rb") as first, right.open("rb") as second:
        while True:
            a = first.read(1024 * 1024)
            b = second.read(1024 * 1024)
            if a != b:
                return False
            if not a:
                return True


class CacheServer(ThreadingHTTPServer):
    """HTTP server state shared by independent request threads."""

    daemon_threads = True

    def server_bind(self) -> None:
        """Bind loopback without HTTPServer's unnecessary reverse-DNS lookup."""

        self.socket.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.socket.bind(self.server_address)
        self.server_address = self.socket.getsockname()
        self.server_name = "127.0.0.1"
        self.server_port = self.server_address[1]

    def __init__(self, address: tuple[str, int], store: FileStore, token: str, latency_ms: float) -> None:
        super().__init__(address, CacheHandler)
        self.store = store
        self.token = token
        self.latency_seconds = latency_ms / 1000
        self.metrics = Metrics()


class CacheHandler(BaseHTTPRequestHandler):
    """Strict subset of the HTTP v1 protocol required by performance runs."""

    protocol_version = "HTTP/1.1"
    server: CacheServer

    def log_message(self, _format: str, *_arguments: Any) -> None:
        return

    def _begin(self, route: str) -> bool:
        if self.headers.get("Authorization") != f"Bearer {self.server.token}":
            self._respond(HTTPStatus.UNAUTHORIZED)
            return False
        if self.headers.get(PROTOCOL_HEADER) != PROTOCOL_VERSION:
            self._respond(HTTPStatus.BAD_REQUEST)
            return False
        self.server.metrics.request(route)
        if self.server.latency_seconds:
            time.sleep(self.server.latency_seconds)
        return True

    def _respond(self, status: HTTPStatus, body: bytes = b"", content_type: str | None = None) -> None:
        self.send_response(status)
        self.send_header(PROTOCOL_HEADER, PROTOCOL_VERSION)
        self.send_header("Content-Length", str(len(body)))
        if content_type:
            self.send_header("Content-Type", content_type)
        self.end_headers()
        if body:
            self.wfile.write(body)

    def _parts(self, prefix: str) -> list[str]:
        path = urlsplit(self.path).path
        if not path.startswith(prefix):
            raise ValueError("unexpected cache route")
        return [part for part in path[len(prefix) :].split("/") if part]

    def do_GET(self) -> None:  # noqa: N802 - stdlib handler API
        parsed = urlsplit(self.path)
        if parsed.path == "/__metrics":
            body = json.dumps(self.server.metrics.snapshot(), sort_keys=True).encode()
            self._respond(HTTPStatus.OK, body, "application/json")
            return
        try:
            if parsed.path.startswith("/v1/actions/"):
                if not self._begin("get_action"):
                    return
                namespace = parse_qs(parsed.query).get("namespace", [""])[0]
                source = self.server.store.action_path(namespace, self._parts("/v1/actions/"))
                if not source.exists():
                    self._respond(HTTPStatus.NOT_FOUND)
                    return
                body = source.read_bytes()
                self.server.metrics.add("metadata_bytes_downloaded", len(body))
                self._respond(HTTPStatus.OK, body, JSON_CONTENT_TYPE)
                return
            if parsed.path.startswith("/v1/blobs/"):
                if not self._begin("read_blob"):
                    return
                source = self.server.store.blob_path(self._parts("/v1/blobs/"))
                if not source.exists():
                    self._respond(HTTPStatus.NOT_FOUND)
                    return
                size = source.stat().st_size
                self.send_response(HTTPStatus.OK)
                self.send_header(PROTOCOL_HEADER, PROTOCOL_VERSION)
                self.send_header("Content-Type", BLOB_CONTENT_TYPE)
                self.send_header("Content-Length", str(size))
                self.end_headers()
                with source.open("rb") as body:
                    shutil.copyfileobj(body, self.wfile, length=1024 * 1024)
                self.server.metrics.add("blob_bytes_downloaded", size)
                return
            self._respond(HTTPStatus.NOT_FOUND)
        except (OSError, ValueError):
            self._respond(HTTPStatus.BAD_REQUEST)

    def do_POST(self) -> None:  # noqa: N802 - stdlib handler API
        if urlsplit(self.path).path != "/v1/blobs/missing":
            self._respond(HTTPStatus.NOT_FOUND)
            return
        if not self._begin("find_missing_blobs"):
            return
        try:
            body = self._read_body()
            request = json.loads(body)
            missing = [blob for blob in request["blobs"] if not self.server.store.blob_path(_blob_parts(blob)).exists()]
            response = json.dumps({"protocol_version": 1, "missing": missing}, separators=(",", ":")).encode()
            self.server.metrics.add("metadata_bytes_uploaded", len(body))
            self.server.metrics.add("metadata_bytes_downloaded", len(response))
            self._respond(HTTPStatus.OK, response, JSON_CONTENT_TYPE)
        except (KeyError, ValueError, OSError, json.JSONDecodeError):
            self._respond(HTTPStatus.BAD_REQUEST)

    def do_PUT(self) -> None:  # noqa: N802 - stdlib handler API
        parsed = urlsplit(self.path)
        try:
            if parsed.path.startswith("/v1/actions/"):
                if not self._begin("write_action"):
                    return
                body = self._read_body()
                request = json.loads(body)
                namespace = parse_qs(parsed.query).get("namespace", [""])[0]
                if request.get("namespace") != namespace:
                    raise ValueError("namespace mismatch")
                result = json.dumps(request["result"], separators=(",", ":"), sort_keys=True).encode()
                destination = self.server.store.action_path(namespace, self._parts("/v1/actions/"))
                status = self.server.store.publish_bytes(destination, result)
                self.server.metrics.add("metadata_bytes_uploaded", len(body))
                self._respond(status)
                return
            if parsed.path.startswith("/v1/blobs/"):
                if not self._begin("write_blob"):
                    return
                parts = self._parts("/v1/blobs/")
                length = int(parts[4])
                destination = self.server.store.blob_path(parts)
                status = self.server.store.publish_stream(destination, self._body_chunks(), length)
                self.server.metrics.add("blob_bytes_uploaded", length)
                self._respond(status)
                return
            self._respond(HTTPStatus.NOT_FOUND)
        except (KeyError, ValueError, OSError, EOFError, json.JSONDecodeError):
            self._respond(HTTPStatus.BAD_REQUEST)

    def _read_body(self) -> bytes:
        length = int(self.headers.get("Content-Length", "-1"))
        if length < 0 or length > 16 * 1024 * 1024:
            raise ValueError("invalid metadata Content-Length")
        body = self.rfile.read(length)
        if len(body) != length:
            raise EOFError("short request body")
        return body

    def _body_chunks(self) -> Any:
        content_length = self.headers.get("Content-Length")
        if content_length is not None:
            remaining = int(content_length)
            while remaining:
                chunk = self.rfile.read(min(1024 * 1024, remaining))
                if not chunk:
                    raise EOFError("short request body")
                remaining -= len(chunk)
                yield chunk
            return
        if self.headers.get("Transfer-Encoding", "").lower() != "chunked":
            raise ValueError("blob upload requires Content-Length or chunked transfer")
        while True:
            line = self.rfile.readline(128)
            if not line.endswith(b"\r\n"):
                raise ValueError("invalid chunk header")
            size = int(line[:-2].split(b";", 1)[0], 16)
            if size == 0:
                while self.rfile.readline(8192) not in {b"\r\n", b""}:
                    pass
                return
            chunk = self.rfile.read(size)
            if len(chunk) != size or self.rfile.read(2) != b"\r\n":
                raise EOFError("short chunked request body")
            yield chunk


def _blob_parts(blob: dict[str, Any]) -> list[str]:
    digest = blob["digest"]
    return [
        str(digest["algorithm"]),
        str(digest["hash"]),
        str(blob["expanded_size_bytes"]),
        str(blob["encoding"]),
        str(blob["encoded_size_bytes"]),
        str(blob["entry_count"]),
    ]


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--certificate", type=Path, required=True)
    parser.add_argument("--key", type=Path, required=True)
    parser.add_argument("--token", required=True)
    parser.add_argument("--latency-ms", type=float, default=0)
    parser.add_argument("--ready-file", type=Path, required=True)
    parser.add_argument("--port", type=int, default=0)
    args = parser.parse_args()

    store = FileStore(args.root)
    server = CacheServer(("127.0.0.1", args.port), store, args.token, args.latency_ms)
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(args.certificate, args.key)
    server.socket = context.wrap_socket(server.socket, server_side=True)
    endpoint = f"https://127.0.0.1:{server.server_port}/"
    args.ready_file.write_text(json.dumps({"endpoint": endpoint}) + "\n", encoding="utf-8")
    try:
        server.serve_forever()
    finally:
        server.server_close()


if __name__ == "__main__":
    main()
