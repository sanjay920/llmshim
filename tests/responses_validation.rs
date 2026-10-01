#![cfg(feature = "proxy")]

#[path = "support/native_post.rs"]
mod native_post;
use axum::http::StatusCode;
use llmshim::{providers::openai_compat::OpenAiCompatible, router::Router};
use native_post::post;
use serde_json::{json, Value};

#[tokio::test]
async fn unsupported_and_malformed_fields_fail_before_dispatch() {
    let mut server = mockito::Server::new_async().await;
    let upstream = server
        .mock("POST", "/chat/completions")
        .expect(0)
        .create_async()
        .await;
    let app = llmshim::proxy::app(
        Router::new().register(
            "local",
            Box::new(OpenAiCompatible::new("local", server.url(), None)),
        ),
        None,
    );
    let (status, error) = post(app.clone(), "/v1/responses", Value::Null).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error["error"]["message"], "request must be an object");
    for (field, value, name) in [
        (
            "previous_response_id",
            json!("resp_old"),
            "previous_response_id",
        ),
        ("conversation", json!({"id": "conv_old"}), "conversation"),
        ("store", json!(true), "store"),
        ("stream", json!("false"), "stream"),
        ("store", Value::Null, "store"),
        ("tools", json!([{"type": "unknown_hosted"}]), "parameters"),
        (
            "tools",
            json!([{"type": "function","name": "weather"}]),
            "parameters",
        ),
        (
            "input",
            json!([{"type": "reasoning","summary": [{"type": "summary_text","text": 3}]}]),
            "reasoning",
        ),
        (
            "input",
            json!([{"role": "user","content": [{"type": "input_file"}]}]),
            "input_file",
        ),
        (
            "input",
            json!([{"type": "reasoning","summary": null}]),
            "reasoning.summary",
        ),
        (
            "input",
            json!([{"type": "reasoning","summary": [],"encrypted_content": 3}]),
            "reasoning.encrypted_content",
        ),
        (
            "input",
            json!([{"type": "reasoning","summary": [],"id": 3}]),
            "reasoning.id",
        ),
        ("include", json!("reasoning.encrypted_content"), "include"),
        ("include", json!(["unsupported"]), "unsupported"),
        (
            "input",
            json!([{"type": "reasoning","summary": [{"type": "text","text": "summary"}]}]),
            "reasoning.summary",
        ),
        ("input", json!(3), "input"),
        ("input", json!([{"role": "tool","content": "hi"}]), "role"),
        (
            "input",
            json!([{"role": "user","content": [{"type": "input_text"}]}]),
            "text",
        ),
        (
            "input",
            json!([
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "input_image",
                            "image_url": "https://example.com/a.png",
                            "detail": "huge"
                        }
                    ]
                }
            ]),
            "detail",
        ),
        ("instructions", json!(3), "instructions"),
        (
            "reasoning",
            json!({"effort": "impossible"}),
            "reasoning.effort",
        ),
        ("reasoning", json!({"summary": "unknown"}), "summary"),
        (
            "text",
            json!({"format": {"type": "json_schema","name": "test"}}),
            "text.format requires schema and name",
        ),
        ("tool_choice", json!({"type": "web_search"}), "tool_choice"),
        ("max_output_tokens", json!("100"), "max_output_tokens"),
        ("max_output_tokens", json!(0), "max_output_tokens"),
        ("temperature", json!("warm"), "temperature"),
        ("temperature", json!(2.1), "temperature"),
        ("top_p", json!(-0.1), "top_p"),
        ("top_p", json!(1.1), "top_p"),
        (
            "input",
            json!([{"type": null,"role": "user","content": "hi"}]),
            "type",
        ),
        (
            "input",
            json!([{"type": "function_call","name": "weather","arguments": "{}"}]),
            "function_call requires call_id",
        ),
        (
            "input",
            json!([{"type": "function_call","call_id": "a","arguments": "{}"}]),
            "function_call requires name",
        ),
        (
            "input",
            json!([{"type": "function_call","call_id": "a","name": "weather","arguments": {}}]),
            "function_call requires arguments text",
        ),
        (
            "input",
            json!([{"type": "function_call_output","output": "hi"}]),
            "function_call_output requires call_id",
        ),
        (
            "input",
            json!([{"type": "function_call_output","call_id": "a","output": {}}]),
            "function_call_output requires output text",
        ),
        ("reasoning", json!(3), "reasoning"),
        ("text", json!(3), "text"),
        ("text", json!({"verbosity": "unknown"}), "text"),
        ("text", json!({"format": {"type": "xml"}}), "text.format"),
        (
            "input",
            json!([{"content": "hi"}]),
            "input message role is required",
        ),
        ("input", json!([{"role": "user","content": 3}]), "content"),
        (
            "input",
            json!([{"type": "function_call","call_id": "","name": "weather","arguments": "{}"}]),
            "function_call requires call_id",
        ),
        (
            "input",
            json!([{"type": "function_call","call_id": "a","name": "","arguments": "{}"}]),
            "function_call requires name",
        ),
        (
            "input",
            json!([{"type": "function_call_output","call_id": "","output": "hi"}]),
            "function_call_output requires call_id",
        ),
        (
            "tools",
            json!([{"type": "function","name": "","parameters": {}}]),
            "function tool requires name and parameters",
        ),
        (
            "tool_choice",
            json!({"type": "function","name": ""}),
            "unsupported tool_choice",
        ),
        ("background", json!(true), "background"),
    ] {
        let mut request = json!({"model": "local/test","input": "hello"});
        request[field] = value;
        let (status, error) = post(app.clone(), "/v1/responses", request).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{field}: {error}");
        assert_eq!(error["error"]["type"], "invalid_request_error");
        if matches!(field, "previous_response_id" | "conversation") {
            assert!(error["error"]["message"]
                .as_str()
                .unwrap()
                .contains("stateless"));
        }
        assert!(
            error["error"]["message"].as_str().unwrap().contains(name),
            "{error}"
        );
    }
    upstream.assert_async().await;
}
