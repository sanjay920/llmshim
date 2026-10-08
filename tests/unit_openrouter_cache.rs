use llmshim::provider::Provider;
use llmshim::providers::openrouter::OpenRouter;
use serde_json::{json, Value};

fn markers(value: &Value) -> Vec<Value> {
    let mut result = Vec::new();
    match value {
        Value::Object(o) => {
            if let Some(c) = o.get("cache_control") {
                result.push(c.clone());
            }
            for v in o.values() {
                result.extend(markers(v));
            }
        }
        Value::Array(a) => {
            for v in a {
                result.extend(markers(v));
            }
        }
        _ => {}
    }
    result
}

#[test]
fn anthropic_model_via_openrouter_receives_cache_markers() {
    let p = OpenRouter::new("test-key".into());
    let req = json!({
        "model": "anthropic/claude-sonnet-5.5",
        "messages": [
            {"role": "user", "content": "part0"},
            {"role": "user", "content": "part1"},
        ],
        "x-cache": {
            "segments": [
                {"upto_message": 0, "stability": "static"},
                {"upto_message": 1, "stability": "session"}
            ]
        }
    });
    let result = p
        .transform_request("anthropic/claude-sonnet-5.5", &req)
        .unwrap();

    // Cache markers should be placed in the OpenAI Chat format body.
    assert_eq!(markers(&result.body).len(), 2);

    // First message should have a 1h marker (static stability).
    assert_eq!(
        result.body["messages"][0]["content"][0]["cache_control"]["ttl"],
        "1h"
    );

    // Second message should have a 5m marker (session stability).
    assert_eq!(
        result.body["messages"][1]["content"][0]["cache_control"]["ttl"],
        "5m"
    );

    // x-cache should not appear in the final body.
    assert!(result.body.get("x-cache").is_none());
}

#[test]
fn google_model_via_openrouter_receives_markers_without_ttl() {
    let p = OpenRouter::new("test-key".into());
    let req = json!({
        "model": "google/gemini-3.8-flash",
        "messages": [
            {"role": "user", "content": "part0"},
        ],
        "x-cache": {
            "segments": [
                {"upto_message": 0, "stability": "session"}
            ]
        }
    });
    let result = p
        .transform_request("google/gemini-3.8-flash", &req)
        .unwrap();

    // Cache markers should be placed.
    assert_eq!(markers(&result.body).len(), 1);

    // Google models don't support ttl, so no ttl field.
    let cache_control = &result.body["messages"][0]["content"][0]["cache_control"];
    assert_eq!(cache_control["type"], "ephemeral");
    assert!(cache_control.get("ttl").is_none());

    // x-cache should not appear in the final body.
    assert!(result.body.get("x-cache").is_none());
}

#[test]
fn google_model_places_marker_only_on_last_segment() {
    let p = OpenRouter::new("test-key".into());
    let req = json!({
        "model": "google/gemini-3.8-flash",
        "messages": [
            {"role": "user", "content": "part0"},
            {"role": "user", "content": "part1"},
            {"role": "user", "content": "part2"},
        ],
        "x-cache": {
            "segments": [
                {"upto_message": 0, "stability": "static"},
                {"upto_message": 1, "stability": "session"},
                {"upto_message": 2, "stability": "session"}
            ]
        }
    });
    let result = p
        .transform_request("google/gemini-3.8-flash", &req)
        .unwrap();

    // Google models place only one marker, at the last segment.
    assert_eq!(markers(&result.body).len(), 1);

    // The marker should be on the last message only.
    assert!(result.body["messages"][0]["content"][0]
        .get("cache_control")
        .is_none());
    assert!(result.body["messages"][1]["content"][0]
        .get("cache_control")
        .is_none());
    assert_eq!(
        result.body["messages"][2]["content"][0]["cache_control"]["type"],
        "ephemeral"
    );
    assert!(result.body["messages"][2]["content"][0]["cache_control"]
        .get("ttl")
        .is_none());
}

#[test]
fn openai_model_via_openrouter_ignores_cache_markers() {
    let p = OpenRouter::new("test-key".into());
    let req = json!({
        "model": "openai/gpt-5.5",
        "messages": [
            {"role": "user", "content": "hi"}
        ],
        "x-cache": {
            "segments": [
                {"upto_message": 0, "stability": "static"}
            ]
        }
    });
    let result = p.transform_request("openai/gpt-5.5", &req).unwrap();

    // No cache markers should be placed for OpenAI models.
    assert!(markers(&result.body).is_empty());

    // x-cache should still be removed.
    assert!(result.body.get("x-cache").is_none());
}

#[test]
fn deepseek_model_via_openrouter_ignores_cache_markers() {
    let p = OpenRouter::new("test-key".into());
    let req = json!({
        "model": "deepseek/deepseek-v4.1-flash",
        "messages": [
            {"role": "user", "content": "hi"}
        ],
        "x-cache": {
            "segments": [
                {"upto_message": 0, "stability": "static"}
            ]
        }
    });
    let result = p
        .transform_request("deepseek/deepseek-v4.1-flash", &req)
        .unwrap();

    // No cache markers should be placed for DeepSeek models.
    assert!(markers(&result.body).is_empty());

    // x-cache should still be removed.
    assert!(result.body.get("x-cache").is_none());
}

#[test]
fn variant_suffix_doesnt_prevent_cache_markers() {
    let p = OpenRouter::new("test-key".into());
    let req = json!({
        "model": "anthropic/claude-sonnet-5.5:nitro",
        "messages": [
            {"role": "user", "content": "hi"}
        ],
        "x-cache": {
            "segments": [
                {"upto_message": 0, "stability": "static"}
            ]
        }
    });
    let result = p
        .transform_request("anthropic/claude-sonnet-5.5:nitro", &req)
        .unwrap();

    // Cache markers should still be placed even with variant suffix.
    assert_eq!(markers(&result.body).len(), 1);
    assert_eq!(
        result.body["messages"][0]["content"][0]["cache_control"]["ttl"],
        "1h"
    );
}

#[test]
fn request_without_cache_annotation_unchanged() {
    let p = OpenRouter::new("test-key".into());
    let req = json!({
        "model": "anthropic/claude-sonnet-5.5",
        "messages": [
            {"role": "user", "content": "hi"}
        ]
    });
    let result = p
        .transform_request("anthropic/claude-sonnet-5.5", &req)
        .unwrap();

    // No cache markers should be placed without x-cache.
    assert!(markers(&result.body).is_empty());
}

#[test]
fn string_content_converted_to_array_for_markers() {
    let p = OpenRouter::new("test-key".into());
    let req = json!({
        "model": "anthropic/claude-sonnet-5.5",
        "messages": [
            {"role": "user", "content": "hello"}
        ],
        "x-cache": {
            "segments": [
                {"upto_message": 0, "stability": "static"}
            ]
        }
    });
    let result = p
        .transform_request("anthropic/claude-sonnet-5.5", &req)
        .unwrap();

    // String content should be converted to array with cache_control on the text block.
    assert!(result.body["messages"][0]["content"].is_array());
    assert_eq!(result.body["messages"][0]["content"][0]["type"], "text");
    assert_eq!(result.body["messages"][0]["content"][0]["text"], "hello");
    assert_eq!(
        result.body["messages"][0]["content"][0]["cache_control"]["ttl"],
        "1h"
    );
}
