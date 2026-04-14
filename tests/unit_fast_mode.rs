use llmshim::provider::Provider;
use llmshim::providers::anthropic::Anthropic;
use llmshim::providers::gemini::Gemini;
use llmshim::providers::openai::OpenAi;
use llmshim::providers::xai::Xai;
use serde_json::json;

// ============================================================
// Anthropic — fast mode
// ============================================================

#[test]
fn anthropic_fast_mode_adds_speed_to_body() {
    let p = Anthropic::new("test-key".into());
    let req = json!({
        "model": "claude-opus-4-6",
        "messages": [{"role": "user", "content": "hi"}],
        "speed": "fast",
    });
    let result = p.transform_request("claude-opus-4-6", &req).unwrap();
    assert_eq!(result.body["speed"], "fast");
}

#[test]
fn anthropic_fast_mode_adds_beta_header() {
    let p = Anthropic::new("test-key".into());
    let req = json!({
        "model": "claude-opus-4-6",
        "messages": [{"role": "user", "content": "hi"}],
        "speed": "fast",
    });
    let result = p.transform_request("claude-opus-4-6", &req).unwrap();
    let beta_header = result
        .headers
        .iter()
        .find(|(k, _)| k == "anthropic-beta")
        .expect("Expected anthropic-beta header");
    assert!(
        beta_header.1.contains("fast-mode-2026-02-01"),
        "Beta header should contain fast-mode-2026-02-01, got: {}",
        beta_header.1
    );
}

#[test]
fn anthropic_fast_mode_combines_beta_headers_with_1m_context() {
    let p = Anthropic::new("test-key".into());
    // claude-opus-4-6 supports 1M context, so both betas should be present
    let req = json!({
        "model": "claude-opus-4-6",
        "messages": [{"role": "user", "content": "hi"}],
        "speed": "fast",
    });
    let result = p.transform_request("claude-opus-4-6", &req).unwrap();
    let beta_header = result
        .headers
        .iter()
        .find(|(k, _)| k == "anthropic-beta")
        .expect("Expected anthropic-beta header");
    assert!(
        beta_header.1.contains("context-1m-2025-08-07"),
        "Beta header should contain context-1m beta, got: {}",
        beta_header.1
    );
    assert!(
        beta_header.1.contains("fast-mode-2026-02-01"),
        "Beta header should contain fast-mode beta, got: {}",
        beta_header.1
    );
}

#[test]
fn anthropic_no_speed_no_fast_mode_header() {
    let p = Anthropic::new("test-key".into());
    let req = json!({
        "model": "claude-opus-4-6",
        "messages": [{"role": "user", "content": "hi"}],
    });
    let result = p.transform_request("claude-opus-4-6", &req).unwrap();
    let beta_header = result.headers.iter().find(|(k, _)| k == "anthropic-beta");
    if let Some((_, val)) = beta_header {
        assert!(
            !val.contains("fast-mode"),
            "Should not contain fast-mode beta without speed param, got: {}",
            val
        );
    }
}

#[test]
fn anthropic_speed_not_fast_no_fast_mode_header() {
    let p = Anthropic::new("test-key".into());
    let req = json!({
        "model": "claude-opus-4-6",
        "messages": [{"role": "user", "content": "hi"}],
        "speed": "standard",
    });
    let result = p.transform_request("claude-opus-4-6", &req).unwrap();
    // "speed" should still be in body (pass through)
    assert_eq!(result.body["speed"], "standard");
    // But fast-mode beta header should NOT be added
    let beta_header = result.headers.iter().find(|(k, _)| k == "anthropic-beta");
    if let Some((_, val)) = beta_header {
        assert!(
            !val.contains("fast-mode"),
            "Should not contain fast-mode beta for speed=standard, got: {}",
            val
        );
    }
}

#[test]
fn anthropic_fast_mode_works_with_non_1m_model() {
    let p = Anthropic::new("test-key".into());
    // Use a model that does NOT support 1M context, so only fast-mode beta should appear
    let req = json!({
        "model": "claude-3-5-sonnet-20241022",
        "messages": [{"role": "user", "content": "hi"}],
        "speed": "fast",
    });
    let result = p
        .transform_request("claude-3-5-sonnet-20241022", &req)
        .unwrap();
    assert_eq!(result.body["speed"], "fast");
    let beta_header = result
        .headers
        .iter()
        .find(|(k, _)| k == "anthropic-beta")
        .expect("Expected anthropic-beta header for fast mode");
    assert_eq!(beta_header.1, "fast-mode-2026-02-01");
}

// ============================================================
// OpenAI — fast mode / priority processing
// ============================================================

#[test]
fn openai_fast_mode_adds_priority() {
    let p = OpenAi::new("test-key".into());
    let req = json!({
        "model": "gpt-5.4",
        "messages": [{"role": "user", "content": "hi"}],
        "speed": "fast",
    });
    let result = p.transform_request("gpt-5.4", &req).unwrap();
    assert_eq!(result.body["priority"]["type"], "default_with_boost");
}

#[test]
fn openai_no_speed_no_priority() {
    let p = OpenAi::new("test-key".into());
    let req = json!({
        "model": "gpt-5.4",
        "messages": [{"role": "user", "content": "hi"}],
    });
    let result = p.transform_request("gpt-5.4", &req).unwrap();
    assert!(
        result.body.get("priority").is_none(),
        "Should not have priority field without speed param"
    );
}

#[test]
fn openai_speed_standard_no_priority() {
    let p = OpenAi::new("test-key".into());
    let req = json!({
        "model": "gpt-5.4",
        "messages": [{"role": "user", "content": "hi"}],
        "speed": "standard",
    });
    let result = p.transform_request("gpt-5.4", &req).unwrap();
    assert!(
        result.body.get("priority").is_none(),
        "Should not have priority field for speed=standard"
    );
}

#[test]
fn openai_fast_mode_does_not_pass_speed_to_body() {
    let p = OpenAi::new("test-key".into());
    let req = json!({
        "model": "gpt-5.4",
        "messages": [{"role": "user", "content": "hi"}],
        "speed": "fast",
    });
    let result = p.transform_request("gpt-5.4", &req).unwrap();
    // OpenAI does not use "speed" field — only "priority"
    assert!(
        result.body.get("speed").is_none(),
        "OpenAI body should not contain 'speed' field"
    );
}

// ============================================================
// Gemini — fast mode silently ignored
// ============================================================

#[test]
fn gemini_fast_mode_ignored() {
    let p = Gemini::new("test-key".into());
    let req = json!({
        "model": "gemini-3-flash-preview",
        "messages": [{"role": "user", "content": "hi"}],
        "speed": "fast",
    });
    let result = p
        .transform_request("gemini-3-flash-preview", &req)
        .unwrap();
    // "speed" should not appear in the Gemini request body
    assert!(
        result.body.get("speed").is_none(),
        "Gemini body should not contain 'speed' field"
    );
    assert!(
        result.body.get("priority").is_none(),
        "Gemini body should not contain 'priority' field"
    );
}

// ============================================================
// xAI — fast mode silently ignored
// ============================================================

#[test]
fn xai_fast_mode_ignored() {
    let p = Xai::new("test-key".into());
    let req = json!({
        "model": "grok-4-1-fast-reasoning",
        "messages": [{"role": "user", "content": "hi"}],
        "speed": "fast",
    });
    let result = p
        .transform_request("grok-4-1-fast-reasoning", &req)
        .unwrap();
    // "speed" should not appear in the xAI request body
    assert!(
        result.body.get("speed").is_none(),
        "xAI body should not contain 'speed' field"
    );
    assert!(
        result.body.get("priority").is_none(),
        "xAI body should not contain 'priority' field"
    );
}

// ============================================================
// Backward compatibility — existing callers work unchanged
// ============================================================

#[test]
fn anthropic_request_without_speed_unchanged() {
    let p = Anthropic::new("test-key".into());
    let req = json!({
        "model": "claude-opus-4-6",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 1024,
    });
    let result = p.transform_request("claude-opus-4-6", &req).unwrap();
    assert!(result.body.get("speed").is_none());
    assert_eq!(result.body["max_tokens"], 1024);
    assert_eq!(result.body["model"], "claude-opus-4-6");
}

#[test]
fn openai_request_without_speed_unchanged() {
    let p = OpenAi::new("test-key".into());
    let req = json!({
        "model": "gpt-5.4",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 1024,
    });
    let result = p.transform_request("gpt-5.4", &req).unwrap();
    assert!(result.body.get("speed").is_none());
    assert!(result.body.get("priority").is_none());
    assert_eq!(result.body["model"], "gpt-5.4");
}
