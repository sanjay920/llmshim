"""Secure auto-managed llmshim proxy lifecycle."""

from __future__ import annotations

import atexit
import json
import os
import platform
import queue
import re
import ssl
import subprocess
import threading
import time
from dataclasses import dataclass
from pathlib import Path
from typing import BinaryIO, Optional
from urllib.parse import urlsplit

import httpx

MANAGED_AUTH_HEADER = "x-llmshim-managed-token"
_MANAGED_PROTOCOL = "llmshim-managed-v1"
_MAX_READINESS_BYTES = 16 * 1024
_MAX_STDERR_BYTES = 32 * 1024
_READINESS_FIELDS = {"protocol", "base_url", "auth_token", "certificate_pem"}
_TOKEN_RE = re.compile(r"^[0-9a-f]{64}$")


@dataclass
class ManagedServer:
    process: subprocess.Popen
    base_url: str
    port: int
    auth_token: str
    certificate_pem: str
    http: httpx.Client


class _BoundedStderr:
    def __init__(self, stream: Optional[BinaryIO]):
        self._stream = stream
        self._buffer = bytearray()
        self._lock = threading.Lock()
        if stream is not None:
            threading.Thread(target=self._drain, daemon=True).start()

    def _drain(self) -> None:
        assert self._stream is not None
        while True:
            chunk = self._stream.read(4096)
            if not chunk:
                return
            with self._lock:
                self._buffer.extend(chunk)
                if len(self._buffer) > _MAX_STDERR_BYTES:
                    del self._buffer[: len(self._buffer) - _MAX_STDERR_BYTES]

    def text(self) -> str:
        with self._lock:
            return bytes(self._buffer).decode(errors="replace")


_state_lock = threading.RLock()
_server_process: Optional[subprocess.Popen] = None
_server_port: Optional[int] = None
_managed_server: Optional[ManagedServer] = None
_cleanup_registered = False


def _find_binary() -> str:
    """Find the bundled, installed, or development llmshim binary."""
    bin_name = "llmshim.exe" if platform.system() == "Windows" else "llmshim"
    pkg_dir = Path(__file__).parent

    import sysconfig

    scripts_dir = sysconfig.get_path("scripts")
    if scripts_dir:
        candidate = Path(scripts_dir) / bin_name
        if candidate.exists() and os.access(str(candidate), os.X_OK):
            return str(candidate)

    bundled = pkg_dir / "bin" / bin_name
    if bundled.exists() and os.access(str(bundled), os.X_OK):
        return str(bundled)

    import shutil

    on_path = shutil.which("llmshim")
    if on_path:
        return on_path

    for root in [pkg_dir.parent.parent.parent, pkg_dir.parent.parent]:
        for build_dir in ["target/release", "target/debug"]:
            candidate = root / build_dir / bin_name
            if candidate.exists() and os.access(str(candidate), os.X_OK):
                return str(candidate)

    raise FileNotFoundError(
        "llmshim binary not found. Install with:\n"
        "  pip install llmshim          (includes the binary)\n"
        "  cargo install llmshim        (from crates.io)\n"
        "  cargo build --release --features proxy  (from source)"
    )


def _read_readiness_line(stream: BinaryIO, timeout: float) -> bytes:
    result: queue.Queue[object] = queue.Queue(maxsize=1)

    def read_line() -> None:
        try:
            result.put(stream.readline(_MAX_READINESS_BYTES + 1))
        except BaseException as error:
            result.put(error)

    threading.Thread(target=read_line, daemon=True).start()
    try:
        value = result.get(timeout=timeout)
    except queue.Empty as error:
        raise RuntimeError("timed out waiting for managed proxy readiness") from error
    if isinstance(value, BaseException):
        raise RuntimeError(f"could not read managed proxy readiness: {value}") from value
    assert isinstance(value, bytes)
    if not value:
        raise RuntimeError(
            "llmshim binary does not support secure managed startup; upgrade llmshim"
        )
    if len(value) > _MAX_READINESS_BYTES or not value.endswith(b"\n"):
        raise RuntimeError("managed proxy readiness record is oversized or unterminated")
    return value


def _parse_readiness(line: bytes) -> tuple[str, int, str, str]:
    try:
        record = json.loads(line)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise RuntimeError("managed proxy returned malformed readiness JSON") from error
    if not isinstance(record, dict) or set(record) != _READINESS_FIELDS:
        raise RuntimeError("managed proxy returned an unsupported readiness record")
    if record.get("protocol") != _MANAGED_PROTOCOL:
        raise RuntimeError(
            "llmshim binary uses an incompatible managed startup protocol; upgrade llmshim"
        )
    base_url = record.get("base_url")
    auth_token = record.get("auth_token")
    certificate_pem = record.get("certificate_pem")
    if not all(isinstance(value, str) for value in (base_url, auth_token, certificate_pem)):
        raise RuntimeError("managed proxy readiness fields have invalid types")
    parsed = urlsplit(base_url)
    try:
        port = parsed.port
    except ValueError as error:
        raise RuntimeError("managed proxy returned an invalid port") from error
    if (
        parsed.scheme != "https"
        or parsed.hostname != "127.0.0.1"
        or parsed.username is not None
        or parsed.password is not None
        or parsed.path not in ("", "/")
        or parsed.query
        or parsed.fragment
        or port is None
        or not (1 <= port <= 65535)
    ):
        raise RuntimeError("managed proxy readiness URL must be loopback HTTPS")
    if not _TOKEN_RE.fullmatch(auth_token):
        raise RuntimeError("managed proxy returned an invalid authentication token")
    if (
        not certificate_pem.startswith("-----BEGIN CERTIFICATE-----\n")
        or not certificate_pem.rstrip().endswith("-----END CERTIFICATE-----")
        or len(certificate_pem.encode()) > 8192
    ):
        raise RuntimeError("managed proxy returned an invalid TLS certificate")
    return base_url.rstrip("/"), port, auth_token, certificate_pem


def _tls_context(certificate_pem: str) -> ssl.SSLContext:
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
    context.minimum_version = ssl.TLSVersion.TLSv1_2
    context.check_hostname = True
    context.verify_mode = ssl.CERT_REQUIRED
    context.load_verify_locations(cadata=certificate_pem)
    return context


def _wait_for_authenticated_health(server: ManagedServer, timeout: float = 10.0) -> None:
    deadline = time.monotonic() + timeout
    headers = {MANAGED_AUTH_HEADER: server.auth_token}
    last_error: Optional[BaseException] = None
    while time.monotonic() < deadline and server.process.poll() is None:
        try:
            response = server.http.get(f"{server.base_url}/health", headers=headers)
            if response.status_code == 200:
                return
            raise RuntimeError(
                f"managed proxy authenticated health check returned HTTP {response.status_code}"
            )
        except (httpx.TransportError, RuntimeError) as error:
            last_error = error
            time.sleep(0.05)
    detail = f": {last_error}" if last_error else ""
    raise RuntimeError(f"managed proxy did not become ready{detail}")


def _terminate_process(process: subprocess.Popen) -> None:
    try:
        process.terminate()
        process.wait(timeout=5)
    except Exception:
        try:
            process.kill()
            process.wait(timeout=1)
        except Exception:
            pass


def _watch_server(server: ManagedServer) -> None:
    server.process.wait()
    global _managed_server, _server_process, _server_port
    with _state_lock:
        if _managed_server is server:
            _managed_server = None
            _server_process = None
            _server_port = None
            server.http.close()


def _start_managed_server() -> ManagedServer:
    binary = _find_binary()
    process = subprocess.Popen(
        [binary, "proxy", "--managed"],
        env=os.environ.copy(),
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    stderr = _BoundedStderr(process.stderr)
    http: Optional[httpx.Client] = None
    try:
        if process.stdout is None:
            raise RuntimeError("managed proxy readiness pipe was not created")
        line = _read_readiness_line(process.stdout, 10.0)
        process.stdout.close()
        base_url, port, auth_token, certificate_pem = _parse_readiness(line)
        http = httpx.Client(
            verify=_tls_context(certificate_pem), timeout=120.0, trust_env=False
        )
        server = ManagedServer(
            process=process,
            base_url=base_url,
            port=port,
            auth_token=auth_token,
            certificate_pem=certificate_pem,
            http=http,
        )
        _wait_for_authenticated_health(server)
        threading.Thread(target=_watch_server, args=(server,), daemon=True).start()
        return server
    except Exception as error:
        if http is not None:
            http.close()
        _terminate_process(process)
        detail = stderr.text()
        if "No providers configured" in detail:
            raise RuntimeError(
                "No API keys configured. Set them with:\n\n"
                "  import llmshim\n"
                "  llmshim.configure(anthropic='sk-ant-...', openai='sk-...')\n\n"
                "Or from the command line:\n  llmshim configure"
            ) from error
        if detail:
            raise RuntimeError(f"{error}\nBinary: {binary}\nstderr: {detail}") from error
        raise


def ensure_managed_server() -> ManagedServer:
    """Return the exact live child and its pinned transport material."""
    global _managed_server, _server_process, _server_port
    _register_cleanup()
    with _state_lock:
        if _managed_server is not None and _managed_server.process.poll() is None:
            return _managed_server
        if _managed_server is not None:
            _managed_server.http.close()
        server = _start_managed_server()
        _managed_server = server
        _server_process = server.process
        _server_port = server.port
        return server


def ensure_server() -> str:
    """Ensure the proxy server is running and return its private HTTPS URL."""
    return ensure_managed_server().base_url


def _stop_server() -> None:
    global _managed_server, _server_process, _server_port
    with _state_lock:
        server = _managed_server
        _managed_server = None
        _server_process = None
        _server_port = None
    if server is not None:
        _terminate_process(server.process)
        server.http.close()


def _register_cleanup() -> None:
    global _cleanup_registered
    if not _cleanup_registered:
        atexit.register(_stop_server)
        _cleanup_registered = True
