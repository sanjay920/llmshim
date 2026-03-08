use llmshim::log::{LogEntry, RequestTimer};
use serde_json::json;
use std::time::Duration;

#[test]
fn log_entry_from_response_extracts_tokens() {
    let resp = json!({
        "id": "resp_123",
        "usage": {
            "prompt_tokens": 100,
            "completion_tokens": 50,
            "reasoning_tokens": 20,
            "total_tokens": 170,
        }
    });
    let entry = LogEntry::from_response(
        "anthropic",
        "claude-sonnet-4-6",
        &resp,
        Duration::from_millis(1500),
    );
    assert_eq!(entry.provider, "anthropic");
    assert_eq!(entry.model, "claude-sonnet-4-6");
    assert_eq!(entry.latency_ms, 1500);
    assert_eq!(entry.input_tokens, 100);
    assert_eq!(entry.output_tokens, 50);
    assert_eq!(entry.reasoning_tokens, 20);
    assert_eq!(entry.total_tokens, 170);
    assert_eq!(entry.status, "ok");
    assert!(entry.error.is_none());
    assert_eq!(entry.request_id, Some("resp_123".into()));
}

#[test]
fn log_entry_from_response_missing_usage() {
    let resp = json!({"id": "x"});
    let entry = LogEntry::from_response("openai", "gpt-5.4", &resp, Duration::from_millis(500));
    assert_eq!(entry.input_tokens, 0);
    assert_eq!(entry.output_tokens, 0);
    assert_eq!(entry.total_tokens, 0);
}

#[test]
fn log_entry_from_response_missing_id() {
    let resp = json!({"usage": {}});
    let entry = LogEntry::from_response(
        "gemini",
        "gemini-3-flash",
        &resp,
        Duration::from_millis(100),
    );
    assert!(entry.request_id.is_none());
}

#[test]
fn log_entry_from_error() {
    let entry = LogEntry::from_error(
        "xai",
        "grok-4-1",
        "connection refused",
        Duration::from_millis(50),
    );
    assert_eq!(entry.provider, "xai");
    assert_eq!(entry.model, "grok-4-1");
    assert_eq!(entry.latency_ms, 50);
    assert_eq!(entry.status, "error");
    assert_eq!(entry.error, Some("connection refused".into()));
    assert_eq!(entry.input_tokens, 0);
    assert_eq!(entry.output_tokens, 0);
    assert!(entry.request_id.is_none());
}

#[test]
fn log_entry_serializes_to_json() {
    let entry = LogEntry::from_response(
        "anthropic",
        "model",
        &json!({"usage": {"prompt_tokens": 5}}),
        Duration::from_millis(100),
    );
    let json_str = serde_json::to_string(&entry).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
    assert_eq!(parsed["provider"], "anthropic");
    assert_eq!(parsed["input_tokens"], 5);
    assert!(parsed["ts"].as_str().is_some());
}

#[test]
fn log_entry_error_serializes_with_error_field() {
    let entry = LogEntry::from_error("openai", "gpt-5.4", "timeout", Duration::from_secs(30));
    let json_str = serde_json::to_string(&entry).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
    assert_eq!(parsed["status"], "error");
    assert_eq!(parsed["error"], "timeout");
}

#[test]
fn request_timer_measures_elapsed() {
    let timer = RequestTimer::start();
    std::thread::sleep(Duration::from_millis(10));
    let elapsed = timer.elapsed();
    assert!(elapsed.as_millis() >= 10);
}
