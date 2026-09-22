from __future__ import annotations

import io
import os
import socket
import threading
from pathlib import Path

import httpx
import pytest

from llmshim import _server


TEST_CERTIFICATE = """-----BEGIN CERTIFICATE-----
MIICwDCCAaigAwIBAgIJAJGXS+9fJK+EMA0GCSqGSIb3DQEBCwUAMBQxEjAQBgNV
BAMMCTEyNy4wLjAuMTAgFw0yNjA5MjIwOTM3MjZaGA8yMTI2MDgyOTA5MzcyNlow
FDESMBAGA1UEAwwJMTI3LjAuMC4xMIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIB
CgKCAQEAzwyCueVAoqCYvYo7dLRgL8PY9vq1RXmWmIbxvqdHTRaacqax+Q6BfJBJ
g2XjSqNzVQ8oYWBP4p6O2v9aSUUDkFk5e6vwzS2+xMk+uMqXBV5LT2PszQHTkJgB
PULZ7lUeAeBqXEMQrM4n5lKEkJPCu2fQOMqOyPLL8JMYXPxtvur4CYWXK+Kce9Fl
9MbQHleHkCugZfJ5oIekmifp3Rgi+dwm7baReMxdCb5ID8mR1x+ZRaCB0s+KrtrD
8H1vHpE1VqJe8pDLAp62XAqVhCehUwNejqRD9gD8AHHlvRzN+Sdwvis9VdSaufPg
sA0QBPN48Wq1NvDZ2ZYRs8dsZxwO+QIDAQABoxMwETAPBgNVHREECDAGhwR/AAAB
MA0GCSqGSIb3DQEBCwUAA4IBAQCXUPGG26iiGM4xvzEb5HWoEBqmzq+yozVgzEXI
imMgAUJ6SHQl+xd0jjtg0jZNErTld2uaZSnKb8u9Rq5F0Z0NLWNvUxBxfs+SDRbe
wI/pvrdOv5r6c5a2OuSB9WUYDK7TmJ2a6HRtPWw28FhUsK4NNAxHcmefq8WFz3fW
30gjjWxq18Ha/98FEsmOsK3F5Lruc3bSzMXMdkqk8zROElC3GXuyXlECDmdM/rOt
lrxSuD2wTiGBjpCNB0/PXzCO0vqJ+rENm5dQqSex1HRXrRutCJstr75y4Vmgk2KN
NYGQAmT/nJGaIzwuJGuRAkmaDQN6uZfj4VwevdyRDY0dfgXk
-----END CERTIFICATE-----
"""


def readiness(**overrides) -> bytes:
    import json

    record = {
        "protocol": "llmshim-managed-v1",
        "base_url": "https://127.0.0.1:41321",
        "auth_token": "a" * 64,
        "certificate_pem": TEST_CERTIFICATE,
    }
    record.update(overrides)
    return json.dumps(record).encode() + b"\n"


def test_readiness_requires_exact_versioned_loopback_schema():
    base_url, port, token, certificate = _server._parse_readiness(readiness())
    assert base_url == "https://127.0.0.1:41321"
    assert port == 41321
    assert token == "a" * 64
    assert certificate == TEST_CERTIFICATE

    with pytest.raises(RuntimeError, match="incompatible"):
        _server._parse_readiness(readiness(protocol="llmshim-managed-v2"))
    with pytest.raises(RuntimeError, match="loopback HTTPS"):
        _server._parse_readiness(readiness(base_url="https://localhost:41321"))
    with pytest.raises(RuntimeError, match="unsupported"):
        _server._parse_readiness(readiness(extra="field"))
    with pytest.raises(RuntimeError, match="malformed"):
        _server._parse_readiness(b"not-json\n")


def test_readiness_is_bounded_and_old_binary_eof_fails_closed():
    with pytest.raises(RuntimeError, match="oversized"):
        _server._read_readiness_line(io.BytesIO(b"x" * (_server._MAX_READINESS_BYTES + 1)), 0.1)
    with pytest.raises(RuntimeError, match="upgrade"):
        _server._read_readiness_line(io.BytesIO(b""), 0.1)


def test_replacement_listener_receives_no_prompt_or_managed_token_before_tls_rejection():
    listener = socket.socket()
    listener.bind(("127.0.0.1", 0))
    listener.listen(1)
    port = listener.getsockname()[1]
    received = bytearray()

    def accept_once():
        connection, _ = listener.accept()
        with connection:
            received.extend(connection.recv(8192))
        listener.close()

    thread = threading.Thread(target=accept_once)
    thread.start()
    secret_prompt = b"prompt-that-must-not-reach-replacement"
    secret_token = "b" * 64
    with httpx.Client(
        verify=_server._tls_context(TEST_CERTIFICATE), trust_env=False, timeout=1.0
    ) as client:
        with pytest.raises(httpx.TransportError):
            client.post(
                f"https://127.0.0.1:{port}/v1/chat",
                headers={_server.MANAGED_AUTH_HEADER: secret_token},
                content=secret_prompt,
            )
    thread.join(timeout=2)
    assert not thread.is_alive()
    assert received
    assert secret_prompt not in received
    assert secret_token.encode() not in received


@pytest.mark.skipif(
    not os.environ.get("LLMSHIM_TEST_BINARY"),
    reason="set LLMSHIM_TEST_BINARY to run the managed child integration test",
)
def test_real_managed_child_ignores_requested_port_and_refreshes_after_exit(monkeypatch):
    import llmshim

    binary = Path(os.environ["LLMSHIM_TEST_BINARY"])
    monkeypatch.setattr(_server, "_find_binary", lambda: str(binary))
    monkeypatch.setenv("VLLM_BASE_URL", "http://127.0.0.1:9/v1")
    unrelated = socket.socket()
    unrelated.bind(("127.0.0.1", 0))
    unrelated.listen(1)
    monkeypatch.setenv("LLMSHIM_PORT", str(unrelated.getsockname()[1]))
    _server._stop_server()

    first = _server.ensure_managed_server()
    assert first.port != unrelated.getsockname()[1]
    unrelated.close()
    _server._terminate_process(first.process)
    first.process.wait(timeout=2)

    replacement = socket.socket()
    replacement.bind(("127.0.0.1", first.port))
    replacement.listen(1)
    received = bytearray()

    def accept_once():
        connection, _ = replacement.accept()
        with connection:
            received.extend(connection.recv(8192))
        replacement.close()

    thread = threading.Thread(target=accept_once)
    thread.start()
    prompt = b"post-exit-prompt-that-must-remain-private"
    with httpx.Client(
        verify=_server._tls_context(first.certificate_pem),
        trust_env=False,
        timeout=1.0,
    ) as stale_http:
        with pytest.raises(httpx.TransportError):
            stale_http.post(
                f"{first.base_url}/v1/chat",
                headers={_server.MANAGED_AUTH_HEADER: first.auth_token},
                content=prompt,
            )
    thread.join(timeout=2)
    assert not thread.is_alive()
    assert prompt not in received
    assert first.auth_token.encode() not in received

    second = _server.ensure_managed_server()
    assert second.process is not first.process
    assert second.port != first.port
    assert llmshim.health()["status"] == "ok"
    _server._stop_server()
