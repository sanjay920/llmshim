"""
Auto-managed llmshim proxy server.

Starts the bundled Rust binary on a random port, waits for it to be ready,
and stops it on process exit. The server is shared across all LlmShim
instances in the same process.
"""

import atexit
import os
import platform
import signal
import socket
import subprocess
import sys
import time
from pathlib import Path

_server_process = None
_server_port = None


def _find_binary() -> str:
    """Find the llmshim binary. Checks in order:
    1. Bundled in the package (clients/python/llmshim/bin/)
    2. On PATH (e.g., installed via cargo install)
    3. In the repo's target/release/ directory
    """
    # 1. Bundled binary
    pkg_dir = Path(__file__).parent
    bin_name = "llmshim.exe" if platform.system() == "Windows" else "llmshim"
    bundled = pkg_dir / "bin" / bin_name
    if bundled.exists() and os.access(str(bundled), os.X_OK):
        return str(bundled)

    # 2. On PATH
    import shutil
    on_path = shutil.which("llmshim")
    if on_path:
        return on_path

    # 3. Repo target directory (development mode)
    repo_root = pkg_dir.parent.parent.parent  # clients/python/llmshim -> repo root
    for build_dir in ["target/release", "target/debug"]:
        candidate = repo_root / build_dir / bin_name
        if candidate.exists() and os.access(str(candidate), os.X_OK):
            return str(candidate)

    raise FileNotFoundError(
        "llmshim binary not found. Install with: cargo install --path . --features proxy\n"
        "Or build with: cargo build --release --features proxy"
    )


def _find_free_port() -> int:
    """Find a free port on localhost."""
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def _wait_for_server(port: int, timeout: float = 10.0) -> bool:
    """Wait until the server is accepting connections."""
    start = time.time()
    while time.time() - start < timeout:
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.5):
                return True
        except (ConnectionRefusedError, OSError):
            time.sleep(0.1)
    return False


def _stop_server():
    """Stop the server process."""
    global _server_process, _server_port
    if _server_process is not None:
        try:
            _server_process.terminate()
            _server_process.wait(timeout=5)
        except Exception:
            try:
                _server_process.kill()
            except Exception:
                pass
        _server_process = None
        _server_port = None


def ensure_server() -> str:
    """Ensure the proxy server is running. Returns the base URL.

    Starts the server automatically if not already running.
    The server is stopped automatically on process exit.
    """
    global _server_process, _server_port

    # Already running?
    if _server_process is not None and _server_process.poll() is None:
        return f"http://127.0.0.1:{_server_port}"

    # Find binary
    binary = _find_binary()

    # Pick a free port
    port = _find_free_port()

    # Start the server
    env = os.environ.copy()
    env["LLMSHIM_HOST"] = "127.0.0.1"
    env["LLMSHIM_PORT"] = str(port)

    _server_process = subprocess.Popen(
        [binary, "proxy"],
        env=env,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
    )

    # Register cleanup
    atexit.register(_stop_server)

    # Also handle SIGTERM
    try:
        original_handler = signal.getsignal(signal.SIGTERM)
        def _handle_sigterm(signum, frame):
            _stop_server()
            if callable(original_handler) and original_handler not in (signal.SIG_DFL, signal.SIG_IGN):
                original_handler(signum, frame)
            sys.exit(0)
        signal.signal(signal.SIGTERM, _handle_sigterm)
    except (ValueError, OSError):
        pass  # Can't set signal handler in some contexts (e.g., threads)

    # Wait for server to be ready
    if not _wait_for_server(port):
        stderr_output = ""
        try:
            stderr_output = _server_process.stderr.read().decode() if _server_process.stderr else ""
        except Exception:
            pass
        _stop_server()
        raise RuntimeError(
            f"llmshim proxy failed to start on port {port}.\n"
            f"Binary: {binary}\n"
            f"stderr: {stderr_output}"
        )

    _server_port = port
    return f"http://127.0.0.1:{port}"
