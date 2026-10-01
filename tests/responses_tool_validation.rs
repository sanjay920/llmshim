#![cfg(feature = "proxy")]

#[path = "support/native_post.rs"]
mod native_post;
use axum::http::StatusCode;
use llmshim::{providers::openai_compat::OpenAiCompatible, reasoning::WireFormat, router::Router};
use native_post::post;
use serde_json::{json, Value};

#[tokio::test]
async fn tool_shapes_and_call_namespaces_have_admission_near_misses() {
    let mut server = mockito::Server::new_async().await;
    let app = llmshim::proxy::app(
        Router::new().register(
            "local",
            Box::new(
                OpenAiCompatible::new("local", server.url(), None)
                    .with_wire(WireFormat::OpenAiResponses),
            ),
        ),
        None,
    );
    let grammar = json!({"type": "custom", "name": "patch", "format": {
        "type": "grammar", "syntax": "regex", "definition": ".*",
    }});
    let function = json!({"type": "function", "name": "read", "parameters": {"type": "object"}});
    let namespace = json!({"type": "namespace", "name": "functions", "tools": [function]});
    let mut malformed_format = grammar.clone();
    malformed_format["format"]["syntax"] = json!("unknown");
    let mut missing_definition = grammar.clone();
    missing_definition["format"]["definition"] = Value::Null;
    let mut wrong_namespace_name = namespace.clone();
    wrong_namespace_name["name"] = json!("");
    let mut wrong_namespace_tools = namespace.clone();
    wrong_namespace_tools["tools"] = json!(3);
    for (valid, invalid) in [
        (
            json!({"tools": [{"type": "web_search"}]}),
            json!({"tools": [{"type": "namespace", "name": "functions",
                "tools": [{"type": "web_search"}]}]}),
        ),
        (
            json!({"tools": [grammar], "tool_choice": {"type": "custom", "name": "patch"}}),
            json!({"tools": [grammar], "tool_choice": {"type": "unknown", "name": "patch"}}),
        ),
        (
            json!({"input": [{"role": "user", "content": [{"type": "input_image",
                "image_url": "https://example.com/image.png", "detail": "original"}]}]}),
            json!({"input": [{"role": "user", "content": [{"type": "input_image",
                "image_url": "https://example.com/image.png", "detail": "unrecognized"}]}]}),
        ),
        (
            json!({"tools": [grammar]}),
            json!({"tools": [malformed_format]}),
        ),
        (
            json!({"tools": [grammar]}),
            json!({"tools": [missing_definition]}),
        ),
        (
            json!({"tools": [namespace]}),
            json!({"tools": [wrong_namespace_name]}),
        ),
        (
            json!({"tools": [namespace]}),
            json!({"tools": [wrong_namespace_tools]}),
        ),
        (
            json!({"tools": [function]}),
            json!({"tools": [function, function]}),
        ),
        (
            json!({"input": [{"type": "additional_tools", "role": "developer",
            "tools": [function]}]}),
            json!({"input": [{"type": "additional_tools",
            "role": "user", "tools": [function]}]}),
        ),
        (
            json!({"input": [
                {"type": "function_call", "call_id": "read", "name": "read",
                    "namespace": "functions", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "read", "output": "ok"},
            ]}),
            json!({"input": [
                {"type": "function_call", "call_id": "read", "name": "read",
                    "namespace": 3, "arguments": "{}"},
                {"type": "function_call_output", "call_id": "read", "output": "ok"},
            ]}),
        ),
        (
            json!({"tools": [namespace], "tool_choice": {"type": "function",
            "name": "read", "namespace": "functions"}}),
            json!({"tools": [namespace], "tool_choice": {"type": "function",
            "name": "read", "namespace": 3}}),
        ),
    ] {
        let upstream = server
            .mock("POST", "/responses")
            .with_body(json!({"id": "response", "status": "completed", "output": []}).to_string())
            .expect(1)
            .create_async()
            .await;
        for (fields, expected) in [(valid, StatusCode::OK), (invalid, StatusCode::BAD_REQUEST)] {
            let mut request = json!({"model": "local/test", "input": "hello"});
            request
                .as_object_mut()
                .unwrap()
                .extend(fields.as_object().unwrap().clone());
            let (status, result) = post(app.clone(), "/v1/responses", request).await;
            assert_eq!(status, expected, "{result}");
        }
        upstream.assert_async().await;
        upstream.remove_async().await;
    }
}

#[tokio::test]
async fn malformed_native_custom_calls_fail_instead_of_becoming_function_calls() {
    let mut server = mockito::Server::new_async().await;
    let app = llmshim::proxy::app(
        Router::new().register(
            "local",
            Box::new(
                OpenAiCompatible::new("local", server.url(), None)
                    .with_wire(WireFormat::OpenAiResponses),
            ),
        ),
        None,
    );
    for (input, namespace, expected) in [
        (json!("patch"), json!("functions"), StatusCode::OK),
        (json!(3), json!("functions"), StatusCode::BAD_GATEWAY),
        (json!("patch"), json!(3), StatusCode::BAD_GATEWAY),
    ] {
        let upstream = server
            .mock("POST", "/responses")
            .with_body(
                json!({"id": "response", "status": "completed", "output": [{
                    "type": "custom_tool_call", "id": "item", "call_id": "patch",
                    "name": "patch", "namespace": namespace, "input": input,
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
                "model": "local/test", "input": "hello", "tools": [{"type": "namespace",
                    "name": "functions", "tools": [{"type": "custom", "name": "patch",
                        "format": {"type": "text"}}]}],
            }),
        )
        .await;
        assert_eq!(status, expected, "{result}");
        upstream.assert_async().await;
        upstream.remove_async().await;
    }
}

#[test]
fn native_assistant_items_share_a_turn_without_weakening_chat_history() {
    use llmshim::provider::Provider;
    let request = json!({"x-responses-controls": {}, "messages": [
        {"role": "assistant", "content": null, "tool_calls": [{"id": "read",
            "type": "function", "function": {"name": "read", "arguments": "{}"}}]},
        {"role": "assistant", "content": "ready"},
        {"role": "tool", "tool_call_id": "read", "content": "ok"},
    ]});
    let native = OpenAiCompatible::new("local", "http://localhost", None)
        .with_wire(WireFormat::OpenAiResponses);
    let sent = native.transform_request("test", &request).unwrap();
    assert_eq!(sent.body["input"][0]["type"], "function_call");
    assert_eq!(sent.body["input"][1]["content"], "ready");
    let mut ordinary = request.clone();
    ordinary
        .as_object_mut()
        .unwrap()
        .remove("x-responses-controls");
    assert!(native.transform_request("test", &ordinary).is_err());
    let chat = OpenAiCompatible::new("local", "http://localhost", None);
    assert!(chat
        .transform_request("test", &request)
        .err()
        .unwrap()
        .to_string()
        .contains("unanswered tool calls"));
    for index in [0, 1] {
        let mut malformed = request.clone();
        malformed["messages"][index]["tool_calls"] = json!({"bad": true});
        assert!(native
            .transform_request("test", &malformed)
            .err()
            .unwrap()
            .to_string()
            .contains("tool_calls must be an array"));
    }
}
