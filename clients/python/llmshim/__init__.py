"""
llmshim — Multi-provider LLM gateway for Python.

Usage:
    from llmshim import LlmShim

    client = LlmShim()
    resp = client.chat("claude-sonnet-4-6", "Hello!")
    print(resp["message"]["content"])

The proxy server starts automatically on first use and stops on exit.
No separate server process needed.
"""

from llmshim._client import LlmShim

__all__ = ["LlmShim"]
__version__ = "0.1.0"
