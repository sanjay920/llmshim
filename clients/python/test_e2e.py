"""
End-to-end test for the llmshim Python package.

Run: python3 test_e2e.py
"""

import sys
import os

sys.path.insert(0, os.path.dirname(__file__))

import llmshim

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


@test("Health check (auto-starts server)")
def _():
    h = llmshim.health()
    assert_true(h["status"] == "ok", f"status ok, providers: {h['providers']}")
    assert_true(len(h["providers"]) > 0, f"{len(h['providers'])} providers")


@test("List models")
def _():
    m = llmshim.models()
    assert_true(len(m) > 0, f"{len(m)} models available")


@test("Chat: simple string")
def _():
    resp = llmshim.chat("claude-sonnet-4-6", "Say pong. Just that word.", max_tokens=100)
    assert_true(resp["provider"] == "anthropic", f"provider: {resp['provider']}")
    assert_true("pong" in resp["message"]["content"].lower(), f"content: {resp['message']['content']}")
    assert_true(resp["latency_ms"] > 0, f"latency: {resp['latency_ms']}ms")


@test("Chat: OpenAI")
def _():
    resp = llmshim.chat("openai/gpt-5.4", "Say pong.", max_tokens=200)
    assert_true(resp["provider"] == "openai", f"provider: {resp['provider']}")


@test("Chat: Gemini")
def _():
    resp = llmshim.chat("gemini/gemini-3-flash-preview", "Say pong.", max_tokens=200)
    assert_true(resp["provider"] == "gemini", f"provider: {resp['provider']}")


@test("Chat: auto-inferred provider")
def _():
    resp = llmshim.chat("claude-sonnet-4-6", "Say ok.", max_tokens=100)
    assert_true(resp["provider"] == "anthropic", "auto-detected anthropic")


@test("Chat: message array with system")
def _():
    resp = llmshim.chat("claude-sonnet-4-6", [
        {"role": "system", "content": "Always respond in one word."},
        {"role": "user", "content": "What color is the sky?"},
    ], max_tokens=100)
    words = resp["message"]["content"].strip().split()
    assert_true(len(words) <= 3, f"short: '{resp['message']['content']}'")


@test("Chat: reasoning")
def _():
    resp = llmshim.chat(
        "claude-sonnet-4-6", "What is 5+3?",
        max_tokens=4000,
        provider_config={"thinking": {"type": "enabled", "budget_tokens": 2000}},
    )
    assert_true(resp.get("reasoning") is not None, "has reasoning")
    assert_true("8" in str(resp["message"]["content"]), f"answer: {resp['message']['content']}")


@test("Chat: multi-model conversation")
def _():
    msgs = [{"role": "user", "content": "Pick a color. One word."}]
    r1 = llmshim.chat("claude-sonnet-4-6", msgs, max_tokens=100)
    assert_true(r1["message"]["content"] is not None, f"Claude: {r1['message']['content']}")

    msgs.append({"role": "assistant", "content": r1["message"]["content"]})
    msgs.append({"role": "user", "content": "Name a fruit that color. One word."})
    r2 = llmshim.chat("openai/gpt-5.4", msgs, max_tokens=200)
    assert_true(r2["provider"] == "openai", f"GPT: {r2['message']['content']}")


@test("Chat: fallback chain")
def _():
    resp = llmshim.chat("anthropic/nonexistent", "Say fallback.", max_tokens=100,
                         fallback=["anthropic/claude-sonnet-4-6"])
    assert_true("fallback" in resp["message"]["content"].lower(), f"got: {resp['message']['content']}")


@test("Stream: basic")
def _():
    text = ""
    got_done = False
    for event in llmshim.stream("claude-sonnet-4-6", "Count 1 to 3.", max_tokens=200):
        if event["type"] == "content":
            text += event.get("text", "")
        if event["type"] == "done":
            got_done = True
    assert_true("1" in text and "3" in text, f"got: {text[:50]}")
    assert_true(got_done, "got done event")


@test("Stream: with reasoning")
def _():
    reasoning = ""
    content = ""
    for event in llmshim.stream("claude-sonnet-4-6", "What is 2+2?",
                                 max_tokens=4000, reasoning_effort="high"):
        if event["type"] == "reasoning":
            reasoning += event.get("text", "")
        if event["type"] == "content":
            content += event.get("text", "")
    assert_true(len(reasoning) > 0, f"reasoning: {reasoning[:50]}...")
    assert_true(len(content) > 0, f"content: {content[:50]}")


@test("Error: unknown provider")
def _():
    try:
        llmshim.chat("unknown/model", "hi", max_tokens=100)
        assert_true(False, "should have raised")
    except Exception as e:
        assert_true("400" in str(e) or "unknown" in str(e).lower(), f"error: {type(e).__name__}")


print(f"\n{'=' * 50}")
print(f"Results: {passed} passed, {failed} failed")
sys.exit(1 if failed > 0 else 0)
