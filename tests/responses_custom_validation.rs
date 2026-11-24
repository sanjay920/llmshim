#![cfg(feature = "proxy")]

#[path = "support/native_post.rs"]
mod native_post;
use axum::http::StatusCode;
use llmshim::{
    providers::openai_compat::OpenAiCompatible, proxy::wire::Receipts, reasoning::WireFormat,
    router::Router,
};
use native_post::post;
use serde_json::{json, Value};
use std::sync::Arc;

fn custom() -> Value {
    json!({"type": "custom", "name": "apply_patch", "description": "Apply a patch",
        "format": {"type": "text"}})
}

#[tokio::test]
async fn plain_custom_calls_require_exact_string_envelopes() {
    let mut server = mockito::Server::new_async().await;
    let directory = tempfile::tempdir().unwrap();
    let app = llmshim::proxy::app(
        Router::new().register(
            "local",
            Box::new(OpenAiCompatible::new("local", server.url(), None)),
        ),
        None,
    )
    .layer(axum::Extension(Arc::new(Receipts::new(
        directory.path().to_owned(),
    ))));
    for (arguments, expected_status) in [
        (json!({"input": "patch\n"}).to_string(), StatusCode::OK),
        (json!({"input": 3}).to_string(), StatusCode::BAD_GATEWAY),
        (
            json!({"input": "patch", "extra": 3}).to_string(),
            StatusCode::BAD_GATEWAY,
        ),
        (
            json!({"other": "patch"}).to_string(),
            StatusCode::BAD_GATEWAY,
        ),
    ] {
        let upstream = server
            .mock("POST", "/chat/completions")
            .match_body(mockito::Matcher::PartialJson(json!({"tools": [{
                "type": "function", "function": {"name": "apply_patch", "parameters": {
                    "type": "object", "properties": {"input": {"type": "string"}},
                    "required": ["input"], "additionalProperties": false,
                }},
            }]})))
            .with_body(
                json!({"id": "response", "choices": [{"index": 0,
                    "finish_reason": "tool_calls", "message": {"content": null, "tool_calls": [{
                        "id": "upstream_patch", "type": "function", "function": {
                            "name": "apply_patch", "arguments": arguments,
                        },
                    }]},
                }]})
                .to_string(),
            )
            .expect(1)
            .create_async()
            .await;
        let (status, result) = post(
            app.clone(),
            "/v1/responses",
            json!({
                "model": "local/test", "input": "hello", "tools": [custom()],
            }),
        )
        .await;
        assert_eq!(status, expected_status, "{result}");
        if status == StatusCode::OK {
            assert_eq!(result["output"][0]["type"], "custom_tool_call");
            assert_eq!(result["output"][0]["input"], "patch\n");
            assert!(result["output"][0].get("namespace").is_none());
        } else {
            assert!(result["error"]["message"]
                .as_str()
                .unwrap()
                .contains("custom tool"));
        }
        upstream.assert_async().await;
        upstream.remove_async().await;
    }
}

#[tokio::test]
async fn namespace_function_identity_remains_function_even_with_input_argument() {
    let mut server = mockito::Server::new_async().await;
    let directory = tempfile::tempdir().unwrap();
    let wire_name = "rt_b9c1ca7960ec4c37f7eb21b3c2ef2e2df8773925a6174652395fdbf7eb94";
    let upstream = server
        .mock("POST", "/chat/completions")
        .match_body(mockito::Matcher::PartialJson(json!({"tools": [{
            "type": "function", "function": {"name": wire_name,
                "description": "functions.apply_patch: Patch operations\n"},
        }]})))
        .with_body(
            json!({"id": "response", "choices": [{"index": 0,
                "finish_reason": "tool_calls", "message": {"content": null, "tool_calls": [{
                    "id": "call_patch", "type": "function", "function": {
                        "name": wire_name, "arguments": "{\"input\":\"patch\"}",
                    },
                }]},
            }]})
            .to_string(),
        )
        .expect(1)
        .create_async()
        .await;
    let app = llmshim::proxy::app(
        Router::new().register(
            "local",
            Box::new(OpenAiCompatible::new("local", server.url(), None)),
        ),
        None,
    )
    .layer(axum::Extension(Arc::new(Receipts::new(
        directory.path().to_owned(),
    ))));
    let (status, result) = post(
        app,
        "/v1/responses",
        json!({
            "model": "local/test", "input": "hello", "tools": [{
                "type": "namespace", "name": "functions", "description": "Patch operations",
                "tools": [{"type": "function", "name": "apply_patch", "parameters": {
                    "type": "object", "properties": {"input": {"type": "string"}},
                }}],
            }],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["output"][0]["type"], "function_call");
    assert_eq!(result["output"][0]["name"], "apply_patch");
    assert_eq!(result["output"][0]["namespace"], "functions");
    assert_eq!(result["output"][0]["arguments"], "{\"input\":\"patch\"}");
    assert!(result["output"][0].get("input").is_none());
    upstream.assert_async().await;
}

#[tokio::test]
async fn structured_tool_outputs_preserve_native_content_and_translate_text() {
    for native in [true, false] {
        let mut server = mockito::Server::new_async().await;
        let path = if native {
            "/responses"
        } else {
            "/chat/completions"
        };
        let parts = if native {
            json!([
                {"type": "input_text", "text": "ok"},
                {"type": "input_image", "image_url": "https://example.com/image.png"},
                {"type": "input_audio", "audio_url": "data:audio/wav;base64,YQ=="},
                {"type": "encrypted_content", "encrypted_content": "opaque"},
            ])
        } else {
            json!([{"type": "input_text", "text": "ok"},
                {"type": "input_text", "text": "next"}])
        };
        let expected_parts = parts.clone();
        let upstream = server
            .mock("POST", path)
            .match_request(move |request| {
                let value: Value = serde_json::from_slice(request.body().unwrap()).unwrap();
                if native {
                    value["input"].as_array().unwrap().last().unwrap()["output"] == expected_parts
                } else {
                    value["messages"].as_array().unwrap().last().unwrap()["content"] == "ok\nnext"
                }
            })
            .with_body(
                if native {
                    json!({"id": "response", "status": "completed", "output": []})
                } else {
                    json!({"id": "response", "choices": [{"message": {"content": "done"},
                "finish_reason": "stop"}]})
                }
                .to_string(),
            )
            .expect(1)
            .create_async()
            .await;
        let app = llmshim::proxy::app(
            Router::new().register(
                "local",
                Box::new(
                    OpenAiCompatible::new("local", server.url(), None).with_wire(if native {
                        WireFormat::OpenAiResponses
                    } else {
                        WireFormat::OpenAiChat
                    }),
                ),
            ),
            None,
        );
        let mut request = json!({"model": "local/test", "tools": [custom()], "input": [
            {"type": "custom_tool_call", "name": "apply_patch", "call_id": "patch",
                "input": "patch"},
            {"type": "custom_tool_call_output", "call_id": "patch", "output": parts},
        ]});
        let (status, result) = post(app.clone(), "/v1/responses", request.clone()).await;
        assert_eq!(status, StatusCode::OK, "{result}");
        for malformed in [
            json!([{"type": "input_text", "text": 3}]),
            json!([{"type": "input_image"}]),
            json!([{"type": "input_audio", "audio_url": 3}]),
            json!([{"type": "encrypted_content", "encrypted_content": 3}]),
        ] {
            request["input"][1]["output"] = malformed;
            let (status, result) = post(app.clone(), "/v1/responses", request.clone()).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{result}");
        }
        if !native {
            for output in [
                json!([{"type": "input_audio", "audio_url": "data:audio/wav;base64,YQ=="}]),
                json!([{"type": "encrypted_content", "encrypted_content": "opaque"}]),
            ] {
                request["input"][1]["output"] = output;
                let (status, result) = post(app.clone(), "/v1/responses", request.clone()).await;
                assert_eq!(status, StatusCode::BAD_REQUEST, "{result}");
                assert!(result["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("requires a native Responses provider"));
            }
        }
        upstream.assert_async().await;
    }
}
