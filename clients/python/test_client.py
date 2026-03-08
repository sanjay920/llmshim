"""
Battle test for the llmshim Python client.
Run: python3 test_client.py
(requires proxy running: cargo run --features proxy --bin llmshim-proxy)
"""

import sys
sys.path.insert(0, ".")

from llmshim import LlmShim

client = LlmShim()

passed = 0
failed = 0


def assert_true(condition, msg):
    global passed, failed
    if condition:
        passed += 1
        print(f"  ✓ {msg}")
    else:
        failed += 1
        print(f"  ✗ {msg}")


def test(name):
    def decorator(fn):
        print(f"\n{name}")
        try:
            fn()
        except Exception as e:
            global failed
            failed += 1
            print(f"  ✗ EXCEPTION: {e}")
    return decorator


@test("Health check")
def _():
    h = client.health()
    assert_true(h["status"] == "ok", "status is ok")
    assert_true(len(h["providers"]) > 0, f"has {len(h['providers'])} providers")


@test("List models")
def _():
    models = client.models()
    assert_true(len(models) > 0, f"got {len(models)} models")
    assert_true("/" in models[0]["id"], "model id has provider prefix")


@test("Chat — simple string message")
def _():
    resp = client.chat("anthropic/claude-sonnet-4-6", "Say 'pong'. Just that word.", max_tokens=100)
    assert_true(resp.provider == "anthropic", f"provider: {resp.provider}")
    assert_true(resp.message.role == "assistant", "role is assistant")
    content = str(resp.message.content).lower()
    assert_true("pong" in content, f"content contains pong: {resp.message.content}")
    assert_true(resp.usage.input_tokens > 0, f"input_tokens: {resp.usage.input_tokens}")
    assert_true(resp.latency_ms > 0, f"latency: {resp.latency_ms}ms")


@test("Chat — message array with system")
def _():
    resp = client.chat(
        "anthropic/claude-sonnet-4-6",
        [
            {"role": "system", "content": "Always respond in exactly one word."},
            {"role": "user", "content": "What color is the sky?"},
        ],
        max_tokens=100,
    )
    words = str(resp.message.content).strip().split()
    assert_true(len(words) <= 3, f"short response: '{resp.message.content}'")


@test("Chat — auto-inferred provider")
def _():
    resp = client.chat("claude-sonnet-4-6", "Say ok.", max_tokens=100)
    assert_true(resp.provider == "anthropic", "auto-detected anthropic")


@test("Chat — OpenAI")
def _():
    resp = client.chat("openai/gpt-5.4", "Say 'pong'.", max_tokens=100)
    assert_true(resp.provider == "openai", f"provider: {resp.provider}")
    assert_true(resp.message.content is not None, "has content")


@test("Chat — Gemini")
def _():
    resp = client.chat("gemini/gemini-3-flash-preview", "Say 'pong'.", max_tokens=200)
    assert_true(resp.provider == "gemini", f"provider: {resp.provider}")
    assert_true(resp.message.content is not None, "has content")


@test("Chat — with reasoning (provider_config)")
def _():
    resp = client.chat(
        "anthropic/claude-sonnet-4-6",
        "What is 5+3?",
        max_tokens=4000,
        provider_config={"thinking": {"type": "enabled", "budget_tokens": 2000}},
    )
    assert_true(resp.reasoning is not None, "has reasoning content")
    assert_true("8" in str(resp.message.content), f"answer: {resp.message.content}")


@test("Chat — multi-model conversation")
def _():
    messages = [{"role": "user", "content": "Pick a color. One word only."}]
    r1 = client.chat("anthropic/claude-sonnet-4-6", messages, max_tokens=100)
    assert_true(r1.message.content is not None, f"Claude: {r1.message.content}")

    messages.append({"role": "assistant", "content": str(r1.message.content)})
    messages.append({"role": "user", "content": "Name a fruit that color. One word only."})
    r2 = client.chat("openai/gpt-5.4", messages, max_tokens=100)
    assert_true(r2.message.content is not None, f"GPT: {r2.message.content}")
    assert_true(r2.provider == "openai", "switched to openai")


@test("Stream — basic")
def _():
    full_text = ""
    got_done = False
    got_usage = False
    for event in client.stream("anthropic/claude-sonnet-4-6", "Count from 1 to 3.", max_tokens=200):
        if event["type"] == "content":
            full_text += event.get("text", "")
        if event["type"] == "done":
            got_done = True
        if event["type"] == "usage":
            got_usage = True
    assert_true("1" in full_text and "3" in full_text, f"got: {full_text[:50]}")
    assert_true(got_done, "received done event")
    assert_true(got_usage, "received usage event")


@test("Stream — with reasoning")
def _():
    reasoning = ""
    content = ""
    for event in client.stream(
        "anthropic/claude-sonnet-4-6", "What is 2+2?",
        max_tokens=4000, reasoning_effort="high"
    ):
        if event["type"] == "reasoning":
            reasoning += event.get("text", "")
        if event["type"] == "content":
            content += event.get("text", "")
    assert_true(len(reasoning) > 0, f"got reasoning: {reasoning[:50]}...")
    assert_true(len(content) > 0, f"got content: {content[:50]}")


@test("Error — unknown provider")
def _():
    try:
        client.chat("unknown/model", "hi", max_tokens=100)
        assert_true(False, "should have thrown")
    except Exception as e:
        err_str = str(e).lower()
        assert_true(
            "unknown" in err_str or "400" in err_str or "unexpected" in err_str,
            f"error raised: {type(e).__name__}"
        )


print(f"\n{'=' * 40}")
print(f"Results: {passed} passed, {failed} failed")
sys.exit(1 if failed > 0 else 0)
