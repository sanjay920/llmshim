#![cfg(feature = "proxy")]

use llmshim::proxy::types::*;
use serde_json::{json, Value};

// We need to test convert functions, but they're in a private module.
// Test them indirectly by exercising the full round-trip through types.

// ============================================================
// ChatRequest → Value round-trip (via deserialization)
// ============================================================

#[test]
fn request_with_config_applies_all_fields() {
    let json = r#"{
        "model": "anthropic/claude-sonnet-4-6",
        "messages": [{"role": "user", "content": "hi"}],
        "config": {
            "max_tokens": 1000,
            "temperature": 0.7,
            "top_p": 0.9,
            "top_k": 40,
            "stop": ["END"],
            "reasoning_effort": "high"
        }
    }"#;
    let req: ChatRequest = serde_json::from_str(json).unwrap();
    assert_eq!(req.model, "anthropic/claude-sonnet-4-6");
    let cfg = req.config.unwrap();
    assert_eq!(cfg.max_tokens, Some(1000));
    assert_eq!(cfg.temperature, Some(0.7));
    assert_eq!(cfg.top_p, Some(0.9));
    assert_eq!(cfg.top_k, Some(40));
    assert_eq!(cfg.stop.as_ref().unwrap().len(), 1);
    assert_eq!(cfg.reasoning_effort.as_deref(), Some("high"));
}

#[test]
fn request_provider_config_passthrough() {
    let json = r#"{
        "model": "anthropic/claude-sonnet-4-6",
        "messages": [{"role": "user", "content": "hi"}],
        "provider_config": {
            "thinking": {"type": "adaptive"},
            "output_config": {"effort": "high"},
            "x-anthropic": {"disable_1m_context": true}
        }
    }"#;
    let req: ChatRequest = serde_json::from_str(json).unwrap();
    let pc = req.provider_config.unwrap();
    assert_eq!(pc["thinking"]["type"], "adaptive");
    assert_eq!(pc["output_config"]["effort"], "high");
    assert_eq!(pc["x-anthropic"]["disable_1m_context"], true);
}

#[test]
fn request_with_fallback() {
    let json = r#"{
        "model": "anthropic/claude-sonnet-4-6",
        "messages": [{"role": "user", "content": "hi"}],
        "fallback": ["openai/gpt-5.4", "gemini/gemini-3-flash-preview"]
    }"#;
    let req: ChatRequest = serde_json::from_str(json).unwrap();
    let fb = req.fallback.unwrap();
    assert_eq!(fb.len(), 2);
    assert_eq!(fb[0], "openai/gpt-5.4");
}

// ============================================================
// ChatResponse shape
// ============================================================

#[test]
fn response_with_tool_calls() {
    let resp = ChatResponse {
        id: "r1".into(),
        model: "gpt-5.4".into(),
        provider: "openai".into(),
        message: ResponseMessage {
            role: "assistant".into(),
            content: Value::Null,
            tool_calls: Some(json!([{
                "id": "call_1",
                "type": "function",
                "function": {"name": "search", "arguments": "{}"}
            }])),
        },
        reasoning: None,
        usage: Usage {
            input_tokens: 10,
            output_tokens: 5,
            reasoning_tokens: 0,
            total_tokens: 15,
        },
        latency_ms: 500,
    };
    let json = serde_json::to_value(&resp).unwrap();
    assert!(json["message"]["content"].is_null());
    assert_eq!(
        json["message"]["tool_calls"][0]["function"]["name"],
        "search"
    );
}

#[test]
fn response_with_reasoning_tokens() {
    let resp = ChatResponse {
        id: "r2".into(),
        model: "gpt-5.4".into(),
        provider: "openai".into(),
        message: ResponseMessage {
            role: "assistant".into(),
            content: json!("42"),
            tool_calls: None,
        },
        reasoning: Some("I calculated...".into()),
        usage: Usage {
            input_tokens: 10,
            output_tokens: 5,
            reasoning_tokens: 50,
            total_tokens: 65,
        },
        latency_ms: 1000,
    };
    let json = serde_json::to_value(&resp).unwrap();
    assert_eq!(json["reasoning"], "I calculated...");
    assert_eq!(json["usage"]["reasoning_tokens"], 50);
}

// ============================================================
// StreamEvent — all types
// ============================================================

#[test]
fn stream_event_content_has_type() {
    let e = StreamEvent::Content { text: "hi".into() };
    let v: Value = serde_json::from_str(&serde_json::to_string(&e).unwrap()).unwrap();
    assert_eq!(v["type"], "content");
    assert_eq!(v["text"], "hi");
}

#[test]
fn stream_event_reasoning_has_type() {
    let e = StreamEvent::Reasoning {
        text: "think".into(),
    };
    let v: Value = serde_json::from_str(&serde_json::to_string(&e).unwrap()).unwrap();
    assert_eq!(v["type"], "reasoning");
}

#[test]
fn stream_event_tool_call_has_all_fields() {
    let e = StreamEvent::ToolCall {
        id: "c1".into(),
        name: "fn1".into(),
        arguments: "{\"a\":1}".into(),
    };
    let v: Value = serde_json::from_str(&serde_json::to_string(&e).unwrap()).unwrap();
    assert_eq!(v["type"], "tool_call");
    assert_eq!(v["id"], "c1");
    assert_eq!(v["name"], "fn1");
    assert_eq!(v["arguments"], "{\"a\":1}");
}

#[test]
fn stream_event_usage_has_tokens() {
    let e = StreamEvent::Usage(Usage {
        input_tokens: 10,
        output_tokens: 5,
        reasoning_tokens: 3,
        total_tokens: 18,
    });
    let v: Value = serde_json::from_str(&serde_json::to_string(&e).unwrap()).unwrap();
    assert_eq!(v["type"], "usage");
    assert_eq!(v["input_tokens"], 10);
    assert_eq!(v["reasoning_tokens"], 3);
}

#[test]
fn stream_event_done_is_minimal() {
    let e = StreamEvent::Done {};
    let v: Value = serde_json::from_str(&serde_json::to_string(&e).unwrap()).unwrap();
    assert_eq!(v["type"], "done");
    assert_eq!(v.as_object().unwrap().len(), 1); // only "type"
}

#[test]
fn stream_event_error_has_message() {
    let e = StreamEvent::Error {
        message: "boom".into(),
    };
    let v: Value = serde_json::from_str(&serde_json::to_string(&e).unwrap()).unwrap();
    assert_eq!(v["type"], "error");
    assert_eq!(v["message"], "boom");
}
