#![cfg(feature = "proxy")]

use llmshim::proxy::types::*;
use serde_json::{json, Value};

// We need to test the convert module, but it's private.
// Test through the public types + serialization instead.

// ============================================================
// ChatRequest deserialization
// ============================================================

#[test]
fn chat_request_minimal() {
    let json = r#"{"model":"gpt-5.4","messages":[{"role":"user","content":"hi"}]}"#;
    let req: ChatRequest = serde_json::from_str(json).unwrap();
    assert_eq!(req.model, "gpt-5.4");
    assert_eq!(req.messages.len(), 1);
    assert_eq!(req.messages[0].role, "user");
    assert!(!req.stream);
    assert!(req.config.is_none());
    assert!(req.provider_config.is_none());
}

#[test]
fn chat_request_with_config() {
    let json = r#"{
        "model": "anthropic/claude-sonnet-4-6",
        "messages": [{"role": "user", "content": "hi"}],
        "config": {
            "max_tokens": 1000,
            "temperature": 0.7,
            "reasoning_effort": "high"
        }
    }"#;
    let req: ChatRequest = serde_json::from_str(json).unwrap();
    let cfg = req.config.unwrap();
    assert_eq!(cfg.max_tokens, Some(1000));
    assert_eq!(cfg.temperature, Some(0.7));
    assert_eq!(cfg.reasoning_effort.as_deref(), Some("high"));
}

#[test]
fn chat_request_with_provider_config() {
    let json = r#"{
        "model": "anthropic/claude-sonnet-4-6",
        "messages": [{"role": "user", "content": "hi"}],
        "provider_config": {
            "thinking": {"type": "adaptive"},
            "output_config": {"effort": "high"}
        }
    }"#;
    let req: ChatRequest = serde_json::from_str(json).unwrap();
    let pc = req.provider_config.unwrap();
    assert_eq!(pc["thinking"]["type"], "adaptive");
}

#[test]
fn chat_request_with_stream() {
    let json = r#"{"model":"gpt-5.4","messages":[{"role":"user","content":"hi"}],"stream":true}"#;
    let req: ChatRequest = serde_json::from_str(json).unwrap();
    assert!(req.stream);
}

#[test]
fn chat_request_multi_turn() {
    let json = r#"{
        "model": "gpt-5.4",
        "messages": [
            {"role": "system", "content": "You are helpful."},
            {"role": "user", "content": "What is Rust?"},
            {"role": "assistant", "content": "Rust is a programming language."},
            {"role": "user", "content": "Tell me more."}
        ]
    }"#;
    let req: ChatRequest = serde_json::from_str(json).unwrap();
    assert_eq!(req.messages.len(), 4);
    assert_eq!(req.messages[0].role, "system");
    assert_eq!(req.messages[2].role, "assistant");
}

#[test]
fn chat_request_with_tool_calls() {
    let json = r#"{
        "model": "gpt-5.4",
        "messages": [
            {"role": "user", "content": "weather?"},
            {
                "role": "assistant",
                "content": null,
                "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "get_weather", "arguments": "{}"}}]
            },
            {"role": "tool", "tool_call_id": "call_1", "content": "Sunny"}
        ]
    }"#;
    let req: ChatRequest = serde_json::from_str(json).unwrap();
    assert_eq!(
        req.messages[1]
            .tool_calls
            .as_ref()
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(req.messages[2].tool_call_id.as_deref(), Some("call_1"));
}

// ============================================================
// ChatResponse serialization
// ============================================================

#[test]
fn chat_response_serializes() {
    let resp = ChatResponse {
        id: "msg_123".into(),
        model: "claude-sonnet-4-6".into(),
        provider: "anthropic".into(),
        message: ResponseMessage {
            role: "assistant".into(),
            content: json!("Hello!"),
            tool_calls: None,
        },
        reasoning: Some("I thought about it...".into()),
        usage: Usage {
            input_tokens: 10,
            output_tokens: 5,
            reasoning_tokens: 0,
            total_tokens: 15,
        },
        latency_ms: 1200,
    };
    let json = serde_json::to_value(&resp).unwrap();
    assert_eq!(json["id"], "msg_123");
    assert_eq!(json["provider"], "anthropic");
    assert_eq!(json["message"]["content"], "Hello!");
    assert_eq!(json["reasoning"], "I thought about it...");
    assert_eq!(json["usage"]["input_tokens"], 10);
    assert_eq!(json["latency_ms"], 1200);
    // reasoning_tokens=0 should be skipped
    assert!(json["usage"].get("reasoning_tokens").is_none());
}

#[test]
fn chat_response_no_reasoning() {
    let resp = ChatResponse {
        id: "r1".into(),
        model: "gpt-5.4".into(),
        provider: "openai".into(),
        message: ResponseMessage {
            role: "assistant".into(),
            content: json!("Hi"),
            tool_calls: None,
        },
        reasoning: None,
        usage: Usage {
            input_tokens: 5,
            output_tokens: 2,
            reasoning_tokens: 0,
            total_tokens: 7,
        },
        latency_ms: 500,
    };
    let json = serde_json::to_value(&resp).unwrap();
    assert!(json.get("reasoning").is_none());
}

// ============================================================
// StreamEvent serialization
// ============================================================

#[test]
fn stream_event_content() {
    let event = StreamEvent::Content {
        text: "Hello".into(),
    };
    let json = serde_json::to_string(&event).unwrap();
    let parsed: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed["type"], "content");
    assert_eq!(parsed["text"], "Hello");
}

#[test]
fn stream_event_reasoning() {
    let event = StreamEvent::Reasoning {
        text: "Thinking...".into(),
    };
    let json = serde_json::to_string(&event).unwrap();
    let parsed: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed["type"], "reasoning");
    assert_eq!(parsed["text"], "Thinking...");
}

#[test]
fn stream_event_tool_call() {
    let event = StreamEvent::ToolCall {
        id: "call_1".into(),
        name: "search".into(),
        arguments: r#"{"q":"rust"}"#.into(),
    };
    let json = serde_json::to_string(&event).unwrap();
    let parsed: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed["type"], "tool_call");
    assert_eq!(parsed["name"], "search");
}

#[test]
fn stream_event_usage() {
    let event = StreamEvent::Usage(Usage {
        input_tokens: 100,
        output_tokens: 50,
        reasoning_tokens: 20,
        total_tokens: 170,
    });
    let json = serde_json::to_string(&event).unwrap();
    let parsed: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed["type"], "usage");
    assert_eq!(parsed["input_tokens"], 100);
    assert_eq!(parsed["reasoning_tokens"], 20);
}

#[test]
fn stream_event_done() {
    let event = StreamEvent::Done {};
    let json = serde_json::to_string(&event).unwrap();
    let parsed: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed["type"], "done");
}

#[test]
fn stream_event_error() {
    let event = StreamEvent::Error {
        message: "something broke".into(),
    };
    let json = serde_json::to_string(&event).unwrap();
    let parsed: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed["type"], "error");
    assert_eq!(parsed["message"], "something broke");
}

// ============================================================
// Models response
// ============================================================

#[test]
fn models_response_serializes() {
    let resp = ModelsResponse {
        models: vec![
            ModelEntry {
                id: "openai/gpt-5.4".into(),
                provider: "openai".into(),
                name: "gpt-5.4".into(),
            },
            ModelEntry {
                id: "anthropic/claude-sonnet-4-6".into(),
                provider: "anthropic".into(),
                name: "claude-sonnet-4-6".into(),
            },
        ],
    };
    let json = serde_json::to_value(&resp).unwrap();
    assert_eq!(json["models"].as_array().unwrap().len(), 2);
    assert_eq!(json["models"][0]["id"], "openai/gpt-5.4");
}

// ============================================================
// Error response
// ============================================================

#[test]
fn error_response_serializes() {
    let resp = ErrorResponse {
        error: ErrorDetail {
            code: "missing_model".into(),
            message: "Missing 'model' field".into(),
        },
    };
    let json = serde_json::to_value(&resp).unwrap();
    assert_eq!(json["error"]["code"], "missing_model");
}
