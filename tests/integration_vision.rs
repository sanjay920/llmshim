/// Integration tests for vision/image support across all providers.
/// Run with: cargo test --test integration_vision -- --ignored
use serde_json::{json, Value};

fn router() -> llmshim::router::Router {
    llmshim::router::Router::from_env()
}

/// A tiny 1x1 yellow PNG pixel as base64.
const TINY_PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8/5+hHgAHggJ/PchI7wAAAABJRU5ErkJggg==";

fn extract_content(resp: &Value) -> String {
    resp["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("")
        .to_lowercase()
}

fn openai_format_image() -> Value {
    json!({"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{}", TINY_PNG_B64)}})
}

fn anthropic_format_image() -> Value {
    json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": TINY_PNG_B64}})
}

fn make_vision_msg(image_block: Value) -> Value {
    json!({
        "role": "user",
        "content": [
            {"type": "text", "text": "Describe this image in one short sentence."},
            image_block
        ]
    })
}

// ============================================================
// Basic vision — each provider, each format
// ============================================================

#[tokio::test]
#[ignore]
async fn anthropic_vision_openai_format() {
    if std::env::var("ANTHROPIC_API_KEY").is_err() {
        return;
    }
    let router = router();
    let req = json!({
        "model": "anthropic/claude-sonnet-4-6",
        "messages": [make_vision_msg(openai_format_image())],
        "max_tokens": 100,
    });
    let resp = llmshim::completion(&router, &req).await.unwrap();
    let content = extract_content(&resp);
    println!("Anthropic (OpenAI format): {}", content);
    assert!(!content.is_empty(), "Expected content");
}

#[tokio::test]
#[ignore]
async fn anthropic_vision_native_format() {
    if std::env::var("ANTHROPIC_API_KEY").is_err() {
        return;
    }
    let router = router();
    let req = json!({
        "model": "anthropic/claude-sonnet-4-6",
        "messages": [make_vision_msg(anthropic_format_image())],
        "max_tokens": 100,
    });
    let resp = llmshim::completion(&router, &req).await.unwrap();
    let content = extract_content(&resp);
    println!("Anthropic (native format): {}", content);
    assert!(!content.is_empty(), "Expected content");
}

#[tokio::test]
#[ignore]
async fn openai_vision_anthropic_format() {
    if std::env::var("OPENAI_API_KEY").is_err() {
        return;
    }
    let router = router();
    let req = json!({
        "model": "openai/gpt-5.4",
        "messages": [make_vision_msg(anthropic_format_image())],
        "max_tokens": 200,
    });
    let resp = llmshim::completion(&router, &req).await.unwrap();
    let content = extract_content(&resp);
    println!("OpenAI (Anthropic format): {}", content);
    assert!(!content.is_empty(), "Expected content");
}

#[tokio::test]
#[ignore]
async fn gemini_vision_openai_format() {
    if std::env::var("GEMINI_API_KEY").is_err() {
        return;
    }
    let router = router();
    let req = json!({
        "model": "gemini/gemini-3-flash-preview",
        "messages": [make_vision_msg(openai_format_image())],
        "max_tokens": 200,
    });
    let resp = llmshim::completion(&router, &req).await.unwrap();
    let content = extract_content(&resp);
    println!("Gemini (OpenAI format): {}", content);
    assert!(!content.is_empty(), "Expected content");
}

#[tokio::test]
#[ignore]
async fn gemini_vision_anthropic_format() {
    if std::env::var("GEMINI_API_KEY").is_err() {
        return;
    }
    let router = router();
    let req = json!({
        "model": "gemini/gemini-3-flash-preview",
        "messages": [make_vision_msg(anthropic_format_image())],
        "max_tokens": 200,
    });
    let resp = llmshim::completion(&router, &req).await.unwrap();
    let content = extract_content(&resp);
    println!("Gemini (Anthropic format): {}", content);
    assert!(!content.is_empty(), "Expected content");
}

// ============================================================
// Vision with reasoning/thinking
// ============================================================

#[tokio::test]
#[ignore]
async fn anthropic_vision_with_thinking() {
    if std::env::var("ANTHROPIC_API_KEY").is_err() {
        return;
    }
    let router = router();
    let req = json!({
        "model": "anthropic/claude-sonnet-4-6",
        "messages": [make_vision_msg(openai_format_image())],
        "max_tokens": 4000,
        "thinking": {"type": "enabled", "budget_tokens": 2000},
    });
    let resp = llmshim::completion(&router, &req).await.unwrap();
    let content = extract_content(&resp);
    let reasoning = resp["choices"][0]["message"].get("reasoning_content");
    println!("Vision+thinking content: {}", content);
    println!(
        "Vision+thinking reasoning: {}",
        reasoning.and_then(|r| r.as_str()).unwrap_or("none")
    );
    assert!(!content.is_empty(), "Expected content");
    assert!(
        reasoning.is_some(),
        "Expected reasoning with thinking enabled"
    );
}

// ============================================================
// Interleaved models with image in conversation
// ============================================================

#[tokio::test]
#[ignore]
async fn vision_interleaved_anthropic_then_gemini() {
    if std::env::var("ANTHROPIC_API_KEY").is_err() || std::env::var("GEMINI_API_KEY").is_err() {
        return;
    }
    let router = router();

    // Turn 1: Anthropic describes the image
    let req1 = json!({
        "model": "anthropic/claude-sonnet-4-6",
        "messages": [make_vision_msg(openai_format_image())],
        "max_tokens": 200,
    });
    let resp1 = llmshim::completion(&router, &req1).await.unwrap();
    let msg1 = resp1["choices"][0]["message"].clone();
    let content1 = extract_content(&resp1);
    println!("Turn 1 (Anthropic): {}", content1);
    assert!(!content1.is_empty());

    // Turn 2: Gemini continues (text-only follow-up)
    let req2 = json!({
        "model": "gemini/gemini-3-flash-preview",
        "messages": [
            make_vision_msg(openai_format_image()),
            msg1,
            {"role": "user", "content": "Based on your description, what mood does this image convey? One word."},
        ],
        "max_tokens": 200,
    });
    let resp2 = llmshim::completion(&router, &req2).await.unwrap();
    let content2 = extract_content(&resp2);
    println!("Turn 2 (Gemini): {}", content2);
    assert!(!content2.is_empty());
}

#[tokio::test]
#[ignore]
async fn vision_interleaved_gemini_then_openai() {
    if std::env::var("GEMINI_API_KEY").is_err() || std::env::var("OPENAI_API_KEY").is_err() {
        return;
    }
    let router = router();

    // Turn 1: Gemini describes the image
    let req1 = json!({
        "model": "gemini/gemini-3-flash-preview",
        "messages": [make_vision_msg(anthropic_format_image())],
        "max_tokens": 200,
    });
    let resp1 = llmshim::completion(&router, &req1).await.unwrap();
    let msg1 = resp1["choices"][0]["message"].clone();
    let content1 = extract_content(&resp1);
    println!("Turn 1 (Gemini): {}", content1);
    assert!(!content1.is_empty());

    // Turn 2: OpenAI follows up
    let req2 = json!({
        "model": "openai/gpt-5.4",
        "messages": [
            {"role": "user", "content": "I showed you an image earlier and you said something about it."},
            msg1,
            {"role": "user", "content": "What else might you say about a tiny 1-pixel image?"},
        ],
        "max_tokens": 200,
    });
    let resp2 = llmshim::completion(&router, &req2).await.unwrap();
    let content2 = extract_content(&resp2);
    println!("Turn 2 (OpenAI): {}", content2);
    assert!(!content2.is_empty());
}

// ============================================================
// Three-provider hop with image
// ============================================================

#[tokio::test]
#[ignore]
async fn vision_three_provider_hop() {
    if std::env::var("ANTHROPIC_API_KEY").is_err()
        || std::env::var("OPENAI_API_KEY").is_err()
        || std::env::var("GEMINI_API_KEY").is_err()
    {
        return;
    }
    let router = router();

    // Turn 1: Anthropic sees image
    let req1 = json!({
        "model": "anthropic/claude-sonnet-4-6",
        "messages": [make_vision_msg(openai_format_image())],
        "max_tokens": 200,
    });
    let resp1 = llmshim::completion(&router, &req1).await.unwrap();
    let msg1 = resp1["choices"][0]["message"].clone();
    println!("Turn 1 (Anthropic): {}", extract_content(&resp1));

    // Turn 2: Gemini continues
    let req2 = json!({
        "model": "gemini/gemini-3-flash-preview",
        "messages": [
            make_vision_msg(openai_format_image()),
            msg1,
            {"role": "user", "content": "Summarize what you see in 3 words."},
        ],
        "max_tokens": 200,
    });
    let resp2 = llmshim::completion(&router, &req2).await.unwrap();
    let msg2 = resp2["choices"][0]["message"].clone();
    println!("Turn 2 (Gemini): {}", extract_content(&resp2));

    // Turn 3: OpenAI wraps up
    let req3 = json!({
        "model": "openai/gpt-5.4",
        "messages": [
            {"role": "user", "content": "We've been discussing a tiny image."},
            msg1,
            {"role": "user", "content": "Another model also saw it."},
            msg2,
            {"role": "user", "content": "Do you agree with both descriptions? Yes or no."},
        ],
        "max_tokens": 200,
    });
    let resp3 = llmshim::completion(&router, &req3).await.unwrap();
    let content3 = extract_content(&resp3);
    println!("Turn 3 (OpenAI): {}", content3);
    assert!(!content3.is_empty());
}

// ============================================================
// Interleaved text + images — position matters
// ============================================================

#[tokio::test]
#[ignore]
async fn anthropic_interleaved_text_image_position() {
    if std::env::var("ANTHROPIC_API_KEY").is_err() {
        return;
    }
    let router = router();

    // Send two identical images with different labels — model should distinguish by position
    let req = json!({
        "model": "anthropic/claude-sonnet-4-6",
        "messages": [{
            "role": "user",
            "content": [
                {"type": "text", "text": "I'll show you two images labeled A and B."},
                {"type": "text", "text": "Image A:"},
                {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{}", TINY_PNG_B64)}},
                {"type": "text", "text": "Image B:"},
                {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{}", TINY_PNG_B64)}},
                {"type": "text", "text": "How many images did I show you? Reply with just the number."}
            ]
        }],
        "max_tokens": 100,
    });
    let resp = llmshim::completion(&router, &req).await.unwrap();
    let content = extract_content(&resp);
    println!("Interleaved test: {}", content);
    assert!(
        content.contains('2'),
        "Expected 2 images recognized, got: {}",
        content
    );
}

#[tokio::test]
#[ignore]
async fn gemini_interleaved_text_image_position() {
    if std::env::var("GEMINI_API_KEY").is_err() {
        return;
    }
    let router = router();

    let req = json!({
        "model": "gemini/gemini-3-flash-preview",
        "messages": [{
            "role": "user",
            "content": [
                {"type": "text", "text": "Image A:"},
                {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{}", TINY_PNG_B64)}},
                {"type": "text", "text": "Image B:"},
                {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{}", TINY_PNG_B64)}},
                {"type": "text", "text": "How many images did I show you? Reply with just the number."}
            ]
        }],
        "max_tokens": 200,
    });
    let resp = llmshim::completion(&router, &req).await.unwrap();
    let content = extract_content(&resp);
    println!("Gemini interleaved: {}", content);
    assert!(content.contains('2'), "Expected 2 images, got: {}", content);
}

#[tokio::test]
#[ignore]
async fn openai_interleaved_text_image_position() {
    if std::env::var("OPENAI_API_KEY").is_err() {
        return;
    }
    let router = router();

    let req = json!({
        "model": "openai/gpt-5.4",
        "messages": [{
            "role": "user",
            "content": [
                {"type": "text", "text": "I'll show you two images labeled A and B."},
                {"type": "text", "text": "Image A:"},
                {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{}", TINY_PNG_B64)}},
                {"type": "text", "text": "Image B:"},
                {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{}", TINY_PNG_B64)}},
                {"type": "text", "text": "How many images did I show you? Reply with just the number."}
            ]
        }],
        "max_tokens": 200,
    });
    let resp = llmshim::completion(&router, &req).await.unwrap();
    let content = extract_content(&resp);
    println!("OpenAI interleaved: {}", content);
    assert!(
        content.contains('2'),
        "Expected 2 images recognized, got: {}",
        content
    );
}

/// xAI Grok models do NOT support vision — verify we get a clear error.
#[tokio::test]
#[ignore]
async fn xai_vision_returns_error() {
    if std::env::var("XAI_API_KEY").is_err() {
        return;
    }
    let router = router();

    let req = json!({
        "model": "xai/grok-4-1-fast-non-reasoning",
        "messages": [{
            "role": "user",
            "content": [
                {"type": "text", "text": "Describe this"},
                {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{}", TINY_PNG_B64)}}
            ]
        }],
        "max_tokens": 200,
    });
    let result = llmshim::completion(&router, &req).await;
    assert!(result.is_err(), "xAI should reject vision requests");
    println!("xAI vision error (expected): {}", result.unwrap_err());
}
