"""
llmshim Python client example.

Usage:
    pip install requests
    cargo run --features proxy --bin llmshim-proxy &
    python examples/clients/python_client.py
"""

import requests
import json

BASE_URL = "http://localhost:3000"


def chat(model: str, messages: list, config: dict = None, provider_config: dict = None) -> dict:
    """Non-streaming chat completion."""
    body = {"model": model, "messages": messages}
    if config:
        body["config"] = config
    if provider_config:
        body["provider_config"] = provider_config

    resp = requests.post(f"{BASE_URL}/v1/chat", json=body)
    resp.raise_for_status()
    return resp.json()


def chat_stream(model: str, messages: list, config: dict = None):
    """Streaming chat completion with typed SSE events."""
    body = {"model": model, "messages": messages}
    if config:
        body["config"] = config

    resp = requests.post(f"{BASE_URL}/v1/chat/stream", json=body, stream=True)
    resp.raise_for_status()

    for line in resp.iter_lines(decode_unicode=True):
        if not line:
            continue
        if line.startswith("event: "):
            event_type = line[7:]
        elif line.startswith("data: "):
            data = json.loads(line[6:])
            yield event_type, data


def list_models() -> list:
    """List available models."""
    resp = requests.get(f"{BASE_URL}/v1/models")
    resp.raise_for_status()
    return resp.json()["models"]


if __name__ == "__main__":
    # List models
    print("Available models:")
    for m in list_models():
        print(f"  {m['id']}")
    print()

    # Non-streaming
    print("=== Non-streaming (Claude Sonnet 4.6) ===")
    result = chat(
        model="anthropic/claude-sonnet-4-6",
        messages=[{"role": "user", "content": "What is Rust? One sentence."}],
        config={"max_tokens": 200},
    )
    print(f"  Provider: {result['provider']}")
    print(f"  Content: {result['message']['content']}")
    print(f"  Tokens: ↑{result['usage']['input_tokens']} ↓{result['usage']['output_tokens']}")
    print(f"  Latency: {result['latency_ms']}ms")
    if result.get("reasoning"):
        print(f"  Reasoning: {result['reasoning'][:100]}...")
    print()

    # Streaming
    print("=== Streaming (Gemini Flash) ===")
    full_text = ""
    for event_type, data in chat_stream(
        model="gemini/gemini-3-flash-preview",
        messages=[{"role": "user", "content": "Write a haiku about Python."}],
        config={"max_tokens": 200},
    ):
        if event_type == "content":
            print(data["text"], end="", flush=True)
            full_text += data["text"]
        elif event_type == "reasoning":
            pass  # skip thinking in this example
        elif event_type == "usage":
            print(f"\n  [↑{data['input_tokens']} ↓{data['output_tokens']}]")
        elif event_type == "done":
            print()

    # Multi-model conversation
    print("=== Multi-model conversation ===")
    messages = [{"role": "user", "content": "Name a color. Just one word."}]

    r1 = chat("anthropic/claude-sonnet-4-6", messages, config={"max_tokens": 100})
    print(f"  Claude: {r1['message']['content']}")
    messages.append({"role": "assistant", "content": r1["message"]["content"]})

    messages.append({"role": "user", "content": "Now name a fruit that color. Just one word."})
    r2 = chat("openai/gpt-5.4", messages, config={"max_tokens": 100})
    print(f"  GPT:    {r2['message']['content']}")
