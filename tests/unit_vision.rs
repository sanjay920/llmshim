use llmshim::vision;
use serde_json::{json, Value};

// ============================================================
// to_anthropic
// ============================================================

#[test]
fn anthropic_from_openai_chat_completions_url() {
    let block = json!({"type": "image_url", "image_url": {"url": "https://example.com/cat.jpg"}});
    let result = vision::to_anthropic(&block).unwrap();
    assert_eq!(result["type"], "image");
    assert_eq!(result["source"]["type"], "url");
    assert_eq!(result["source"]["url"], "https://example.com/cat.jpg");
}

#[test]
fn anthropic_from_openai_chat_completions_base64() {
    let block = json!({"type": "image_url", "image_url": {"url": "data:image/png;base64,abc123"}});
    let result = vision::to_anthropic(&block).unwrap();
    assert_eq!(result["type"], "image");
    assert_eq!(result["source"]["type"], "base64");
    assert_eq!(result["source"]["media_type"], "image/png");
    assert_eq!(result["source"]["data"], "abc123");
}

#[test]
fn anthropic_from_responses_api_url() {
    let block = json!({"type": "input_image", "image_url": "https://example.com/dog.png"});
    let result = vision::to_anthropic(&block).unwrap();
    assert_eq!(result["type"], "image");
    assert_eq!(result["source"]["type"], "url");
    assert_eq!(result["source"]["url"], "https://example.com/dog.png");
}

#[test]
fn anthropic_from_responses_api_base64() {
    let block = json!({"type": "input_image", "image_url": "data:image/jpeg;base64,xyz789"});
    let result = vision::to_anthropic(&block).unwrap();
    assert_eq!(result["source"]["type"], "base64");
    assert_eq!(result["source"]["media_type"], "image/jpeg");
    assert_eq!(result["source"]["data"], "xyz789");
}

#[test]
fn anthropic_passthrough_native() {
    let block = json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "native"}});
    let result = vision::to_anthropic(&block).unwrap();
    assert_eq!(result["source"]["data"], "native");
}

#[test]
fn anthropic_unknown_type_returns_none() {
    let block = json!({"type": "audio", "data": "..."});
    assert!(vision::to_anthropic(&block).is_none());
}

// ============================================================
// to_gemini
// ============================================================

#[test]
fn gemini_from_openai_chat_completions_base64() {
    let block = json!({"type": "image_url", "image_url": {"url": "data:image/jpeg;base64,img123"}});
    let result = vision::to_gemini(&block).unwrap();
    assert_eq!(result["inline_data"]["mime_type"], "image/jpeg");
    assert_eq!(result["inline_data"]["data"], "img123");
}

#[test]
fn gemini_from_openai_chat_completions_url_fallback() {
    // Gemini doesn't support URL images inline — should fallback to text
    let block = json!({"type": "image_url", "image_url": {"url": "https://example.com/cat.jpg"}});
    let result = vision::to_gemini(&block).unwrap();
    assert!(result["text"]
        .as_str()
        .unwrap()
        .contains("https://example.com/cat.jpg"));
}

#[test]
fn gemini_from_responses_api_base64() {
    let block = json!({"type": "input_image", "image_url": "data:image/png;base64,gemdata"});
    let result = vision::to_gemini(&block).unwrap();
    assert_eq!(result["inline_data"]["mime_type"], "image/png");
    assert_eq!(result["inline_data"]["data"], "gemdata");
}

#[test]
fn gemini_from_anthropic_base64() {
    let block = json!({"type": "image", "source": {"type": "base64", "media_type": "image/webp", "data": "webpdata"}});
    let result = vision::to_gemini(&block).unwrap();
    assert_eq!(result["inline_data"]["mime_type"], "image/webp");
    assert_eq!(result["inline_data"]["data"], "webpdata");
}

#[test]
fn gemini_from_anthropic_url_fallback() {
    let block =
        json!({"type": "image", "source": {"type": "url", "url": "https://example.com/img.png"}});
    let result = vision::to_gemini(&block).unwrap();
    assert!(result["text"]
        .as_str()
        .unwrap()
        .contains("https://example.com/img.png"));
}

// ============================================================
// to_openai
// ============================================================

#[test]
fn openai_passthrough_input_image() {
    let block = json!({"type": "input_image", "image_url": "https://example.com/img.jpg"});
    let result = vision::to_openai(&block).unwrap();
    assert_eq!(result["type"], "input_image");
    assert_eq!(result["image_url"], "https://example.com/img.jpg");
}

#[test]
fn openai_from_chat_completions_format() {
    let block = json!({"type": "image_url", "image_url": {"url": "https://example.com/img.jpg"}});
    let result = vision::to_openai(&block).unwrap();
    assert_eq!(result["type"], "input_image");
    assert_eq!(result["image_url"], "https://example.com/img.jpg");
}

#[test]
fn openai_from_anthropic_base64() {
    let block = json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "abc"}});
    let result = vision::to_openai(&block).unwrap();
    assert_eq!(result["type"], "input_image");
    assert_eq!(result["image_url"], "data:image/png;base64,abc");
}

#[test]
fn openai_from_anthropic_url() {
    let block =
        json!({"type": "image", "source": {"type": "url", "url": "https://example.com/img.jpg"}});
    let result = vision::to_openai(&block).unwrap();
    assert_eq!(result["type"], "input_image");
    assert_eq!(result["image_url"], "https://example.com/img.jpg");
}

// ============================================================
// translate_content_blocks
// ============================================================

#[test]
fn translate_blocks_string_passthrough() {
    let content = json!("Just a string");
    let result = vision::translate_content_blocks(&content, vision::to_anthropic);
    assert_eq!(result, "Just a string");
}

#[test]
fn translate_blocks_null_passthrough() {
    let content = Value::Null;
    let result = vision::translate_content_blocks(&content, vision::to_anthropic);
    assert!(result.is_null());
}

#[test]
fn translate_blocks_text_preserved() {
    let content = json!([{"type": "text", "text": "Hello!"}]);
    let result = vision::translate_content_blocks(&content, vision::to_anthropic);
    assert_eq!(result[0]["type"], "text");
    assert_eq!(result[0]["text"], "Hello!");
}

#[test]
fn translate_blocks_mixed_text_and_image() {
    let content = json!([
        {"type": "text", "text": "What's in this image?"},
        {"type": "image_url", "image_url": {"url": "data:image/jpeg;base64,abc123"}}
    ]);
    let result = vision::translate_content_blocks(&content, vision::to_anthropic);
    let blocks = result.as_array().unwrap();
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0]["type"], "text");
    assert_eq!(blocks[1]["type"], "image");
    assert_eq!(blocks[1]["source"]["type"], "base64");
}

#[test]
fn translate_blocks_unknown_type_passthrough() {
    let content = json!([{"type": "custom_widget", "data": "something"}]);
    let result = vision::translate_content_blocks(&content, vision::to_anthropic);
    assert_eq!(result[0]["type"], "custom_widget");
}

// ============================================================
// Provider integration — image in transform_request
// ============================================================

#[test]
fn anthropic_transforms_image_in_message() {
    use llmshim::provider::Provider;
    use llmshim::providers::anthropic::Anthropic;

    let p = Anthropic::new("key".into());
    let req = json!({
        "model": "claude-sonnet-4-6",
        "messages": [{
            "role": "user",
            "content": [
                {"type": "text", "text": "Describe this image"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,testdata"}}
            ]
        }],
        "max_tokens": 100,
    });
    let result = p.transform_request("claude-sonnet-4-6", &req).unwrap();
    let content = result.body["messages"][0]["content"].as_array().unwrap();
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[1]["type"], "image");
    assert_eq!(content[1]["source"]["type"], "base64");
    assert_eq!(content[1]["source"]["data"], "testdata");
}

#[test]
fn gemini_transforms_image_in_message() {
    use llmshim::provider::Provider;
    use llmshim::providers::gemini::Gemini;

    let p = Gemini::new("key".into());
    let req = json!({
        "model": "gemini-3-flash-preview",
        "messages": [{
            "role": "user",
            "content": [
                {"type": "text", "text": "What is this?"},
                {"type": "image_url", "image_url": {"url": "data:image/jpeg;base64,imgdata"}}
            ]
        }],
    });
    let result = p.transform_request("gemini-3-flash-preview", &req).unwrap();
    let parts = result.body["contents"][0]["parts"].as_array().unwrap();
    assert_eq!(parts[0]["text"], "What is this?");
    assert_eq!(parts[1]["inline_data"]["mime_type"], "image/jpeg");
    assert_eq!(parts[1]["inline_data"]["data"], "imgdata");
}

#[test]
fn openai_transforms_image_in_message() {
    use llmshim::provider::Provider;
    use llmshim::providers::openai::OpenAi;

    let p = OpenAi::new("key".into());
    let req = json!({
        "model": "gpt-5.4",
        "messages": [{
            "role": "user",
            "content": [
                {"type": "text", "text": "Describe"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "abc"}}
            ]
        }],
    });
    let result = p.transform_request("gpt-5.4", &req).unwrap();
    let input = result.body["input"].as_array().unwrap();
    let content = input[0]["content"].as_array().unwrap();
    assert_eq!(content[0]["type"], "input_text"); // "text" → "input_text" for Responses API
    assert_eq!(content[1]["type"], "input_image");
    assert_eq!(content[1]["image_url"], "data:image/png;base64,abc");
}

// ============================================================
// Interleaved text + image ordering is preserved
// ============================================================

#[test]
fn anthropic_preserves_interleaved_text_image_order() {
    use llmshim::provider::Provider;
    use llmshim::providers::anthropic::Anthropic;

    let p = Anthropic::new("key".into());
    let req = json!({
        "model": "claude-sonnet-4-6",
        "messages": [{
            "role": "user",
            "content": [
                {"type": "text", "text": "First look at this"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,img1data"}},
                {"type": "text", "text": "Now compare with this"},
                {"type": "image_url", "image_url": {"url": "data:image/jpeg;base64,img2data"}},
                {"type": "text", "text": "Which is better?"}
            ]
        }],
        "max_tokens": 100,
    });
    let result = p.transform_request("claude-sonnet-4-6", &req).unwrap();
    let content = result.body["messages"][0]["content"].as_array().unwrap();

    // Verify order: text, image, text, image, text
    assert_eq!(content.len(), 5);
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[0]["text"], "First look at this");
    assert_eq!(content[1]["type"], "image");
    assert_eq!(content[1]["source"]["data"], "img1data");
    assert_eq!(content[2]["type"], "text");
    assert_eq!(content[2]["text"], "Now compare with this");
    assert_eq!(content[3]["type"], "image");
    assert_eq!(content[3]["source"]["data"], "img2data");
    assert_eq!(content[4]["type"], "text");
    assert_eq!(content[4]["text"], "Which is better?");
}

#[test]
fn gemini_preserves_interleaved_text_image_order() {
    use llmshim::provider::Provider;
    use llmshim::providers::gemini::Gemini;

    let p = Gemini::new("key".into());
    let req = json!({
        "model": "gemini-3-flash-preview",
        "messages": [{
            "role": "user",
            "content": [
                {"type": "text", "text": "Image A:"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,aaa"}},
                {"type": "text", "text": "Image B:"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,bbb"}}
            ]
        }],
    });
    let result = p.transform_request("gemini-3-flash-preview", &req).unwrap();
    let parts = result.body["contents"][0]["parts"].as_array().unwrap();

    // Verify order: text, inline_data, text, inline_data
    assert_eq!(parts.len(), 4);
    assert_eq!(parts[0]["text"], "Image A:");
    assert!(parts[1].get("inline_data").is_some());
    assert_eq!(parts[1]["inline_data"]["data"], "aaa");
    assert_eq!(parts[2]["text"], "Image B:");
    assert!(parts[3].get("inline_data").is_some());
    assert_eq!(parts[3]["inline_data"]["data"], "bbb");
}

#[test]
fn openai_preserves_interleaved_text_image_order() {
    use llmshim::provider::Provider;
    use llmshim::providers::openai::OpenAi;

    let p = OpenAi::new("key".into());
    let req = json!({
        "model": "gpt-5.4",
        "messages": [{
            "role": "user",
            "content": [
                {"type": "text", "text": "Compare:"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,x1"}},
                {"type": "text", "text": "vs"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,x2"}}
            ]
        }],
    });
    let result = p.transform_request("gpt-5.4", &req).unwrap();
    let input = result.body["input"].as_array().unwrap();
    let content = input[0]["content"].as_array().unwrap();

    // Verify order: input_text, input_image, input_text, input_image
    assert_eq!(content.len(), 4);
    assert_eq!(content[0]["type"], "input_text");
    assert_eq!(content[0]["text"], "Compare:");
    assert_eq!(content[1]["type"], "input_image");
    assert_eq!(content[2]["type"], "input_text");
    assert_eq!(content[2]["text"], "vs");
    assert_eq!(content[3]["type"], "input_image");
}
