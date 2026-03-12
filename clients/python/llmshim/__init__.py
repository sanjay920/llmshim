"""
llmshim — Multi-provider LLM gateway for Python.

Usage:
    import llmshim

    llmshim.configure(anthropic="sk-ant-...", openai="sk-...")
    resp = llmshim.chat("claude-sonnet-4-6", "Hello!")
    print(resp["message"]["content"])

The proxy server starts automatically on first use and stops on exit.
"""

from llmshim._client import chat, stream, models, health, configure

__all__ = ["chat", "stream", "models", "health", "configure"]
__version__ = "0.1.2"
