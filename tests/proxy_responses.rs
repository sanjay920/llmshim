#![cfg(feature = "proxy")]

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use llmshim::providers::{
    anthropic::Anthropic, gemini::Gemini, openai::OpenAi, openai_compat::OpenAiCompatible, xai::Xai,
};
use llmshim::router::Router;
use serde_json::{json, Value};
use tower::ServiceExt;

async fn post(app: axum::Router, body: Value) -> (StatusCode, Value) {
    let receipts = llmshim::proxy::wire::Receipts::new(std::path::PathBuf::from(format!(
        "target/responses-receipts-{}",
        std::process::id()
    )));
    let response = app
        .layer(axum::Extension(std::sync::Arc::new(receipts)))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 100_000).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn stateless_text_crosses_each_provider_family() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/responses-api-reference.json")).unwrap();
    let text = fixture["output"][0]["content"][0]["text"].as_str().unwrap();
    for family in ["openai", "anthropic", "gemini", "local", "xai"] {
        let mut server = mockito::Server::new_async().await;
        let (provider, path, response): (Box<dyn llmshim::provider::Provider>, &str, Value) =
            match family {
                "openai" => (
                    Box::new(OpenAi::new("key".into()).with_base_url(server.url())),
                    "/responses",
                    fixture.clone(),
                ),
                "xai" => (
                    Box::new(Xai::new("key".into()).with_base_url(server.url())),
                    "/responses",
                    fixture.clone(),
                ),
                "anthropic" => (
                    Box::new(Anthropic::new("key".into()).with_base_url(server.url())),
                    "/messages",
                    json!({"id":"msg_test","type":"message","role":"assistant","content":[{"type":"text","text":text}],"stop_reason":"end_turn","usage":{"input_tokens":12,"output_tokens":3,"cache_read_input_tokens":4}}),
                ),
                "gemini" => (
                    Box::new(Gemini::new("key".into()).with_base_url(server.url())),
                    "/models/test:generateContent?key=key",
                    json!({"candidates":[{"content":{"role":"model","parts":[{"text":text}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":12,"candidatesTokenCount":3,"totalTokenCount":15,"cachedContentTokenCount":4}}),
                ),
                _ => (
                    Box::new(OpenAiCompatible::new("local", server.url(), None)),
                    "/chat/completions",
                    json!({"id":"chat_test","choices":[{"message":{"role":"assistant","content":text},"finish_reason":"stop"}],"usage":{"prompt_tokens":12,"completion_tokens":3,"total_tokens":15,"prompt_tokens_details":{"cached_tokens":4}}}),
                ),
            };
        let upstream = server
            .mock("POST", path)
            .match_body(mockito::Matcher::Regex("hello".into()))
            .with_body(response.to_string())
            .expect(1)
            .create_async()
            .await;
        let app = llmshim::proxy::app(Router::new().register(family, provider), None);
        let (status, response) = post(
            app.clone(),
            json!({"model":format!("{family}/test"),"input":"hello","store":false,"stream":false}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{family}: {response}");
        assert_eq!(response["object"], "response");
        assert_eq!(response["status"], "completed");
        assert_eq!(response["output"][0]["content"][0]["type"], "output_text");
        assert_eq!(response["output"][0]["content"][0]["text"], text);
        assert_eq!(response["store"], false);
        if matches!(family, "anthropic" | "gemini" | "local") {
            assert_eq!(
                response["usage"]["input_tokens_details"]["cached_tokens"],
                4
            );
        }
        assert!(response["usage"]["input_tokens"].as_u64().unwrap() > 0);
        assert!(response["usage"]["output_tokens_details"]["reasoning_tokens"].is_u64());
        let (status, error) = post(
            app,
            json!({"model":format!("{family}/test"),"input":"hello","store":true}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("store"));
        upstream.assert_async().await;
    }
}

#[tokio::test]
async fn tools_round_trip_and_translate_controls() {
    let mut server = mockito::Server::new_async().await;
    let tool = json!({"type":"function","name":"weather","description":"Weather","parameters":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]},"strict":true});
    let first = server.mock("POST","/chat/completions").match_body(mockito::Matcher::PartialJson(json!({
        "messages":[{"role":"system","content":"Be brief"},{"role":"user","content":"Paris?"}],
        "max_tokens":100,"temperature":0.2,"top_p":0.9,
        "tool_choice":{"type":"function","function":{"name":"weather"}},
        "tools":[{"type":"function","function":{"name":"weather","description":"Weather","parameters":tool["parameters"],"strict":true}}]
    }))).with_body(json!({"id":"chat_tool","choices":[{"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_weather","type":"function","function":{"name":"weather","arguments":"{\"city\":\"Paris\"}"}}]},"finish_reason":"tool_calls"}],"usage":{}}).to_string()).expect(1).create_async().await;
    let app = llmshim::proxy::app(
        Router::new().register(
            "local",
            Box::new(OpenAiCompatible::new("local", server.url(), None)),
        ),
        None,
    );
    let request = json!({"model":"local/test","input":"Paris?","instructions":"Be brief","tools":[tool],"tool_choice":{"type":"function","name":"weather"},"max_output_tokens":100,"temperature":0.2,"top_p":0.9});
    let (status, response) = post(app.clone(), request).await;
    assert_eq!(status, StatusCode::OK, "{response}");
    let call = response["output"][0].clone();
    assert_eq!(call["type"], "function_call");
    assert!(call["call_id"].as_str().unwrap().starts_with("call_ls_"));
    let call_id = call["call_id"].clone();
    assert_eq!(call["name"], "weather");
    assert_eq!(call["arguments"], "{\"city\":\"Paris\"}");
    first.assert_async().await;
    let second = server.mock("POST","/chat/completions").match_body(mockito::Matcher::PartialJson(json!({"messages":[
        {"role":"user","content":"Paris?"},
        {"role":"assistant","content":null,"tool_calls":[{"id":"call_weather","type":"function","function":{"name":"weather","arguments":"{\"city\":\"Paris\"}"}}]},
        {"role":"tool","tool_call_id":"call_weather","content":"sunny"}
    ]}))).with_body(json!({"id":"chat_answer","choices":[{"message":{"role":"assistant","content":"Sunny in Paris"},"finish_reason":"stop"}],"usage":{}}).to_string()).expect(1).create_async().await;
    let mut followup = json!({"model":"local/test","input":[{"role":"user","content":"Paris?"},call,{"type":"function_call_output","call_id":call_id,"output":"sunny"}]});
    let (status, answer) = post(app.clone(), followup.clone()).await;
    assert_eq!(status, StatusCode::OK, "{answer}");
    assert_eq!(answer["output"][0]["content"][0]["text"], "Sunny in Paris");
    followup["input"][2]["call_id"] = json!("orphan");
    let (status, _) = post(app, followup).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    second.assert_async().await;
}

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
    let (status, error) = post(app.clone(), Value::Null).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(error["error"]["message"], "request must be an object");
    for (field, value, name) in [
        (
            "previous_response_id",
            json!("resp_old"),
            "previous_response_id",
        ),
        ("conversation", json!({"id":"conv_old"}), "conversation"),
        ("stream", json!(true), "stream"),
        ("store", json!(true), "store"),
        ("stream", json!("false"), "stream"),
        ("store", Value::Null, "store"),
        ("tools", json!([{"type":"web_search"}]), "web_search"),
        (
            "tools",
            json!([{"type":"function","name":"weather"}]),
            "parameters",
        ),
        (
            "input",
            json!([{"type":"reasoning","summary":[]}]),
            "reasoning",
        ),
        (
            "input",
            json!([{"role":"user","content":[{"type":"input_file"}]}]),
            "input_file",
        ),
        ("input", json!(3), "input"),
        ("input", json!([{"role":"tool","content":"hi"}]), "role"),
        (
            "input",
            json!([{"role":"user","content":[{"type":"input_text"}]}]),
            "text",
        ),
        (
            "input",
            json!([{"role":"user","content":[{"type":"input_image","image_url":"https://example.com/a.png","detail":"huge"}]}]),
            "detail",
        ),
        ("instructions", json!(3), "instructions"),
        (
            "reasoning",
            json!({"effort":"impossible"}),
            "reasoning.effort",
        ),
        ("reasoning", json!({"summary":"auto"}), "summary"),
        (
            "text",
            json!({"format":{"type":"json_schema","name":"test"}}),
            "text.format requires schema and name",
        ),
        ("tool_choice", json!({"type":"web_search"}), "tool_choice"),
        ("max_output_tokens", json!("100"), "max_output_tokens"),
        ("max_output_tokens", json!(0), "max_output_tokens"),
        ("temperature", json!("warm"), "temperature"),
        ("temperature", json!(2.1), "temperature"),
        ("top_p", json!(-0.1), "top_p"),
        ("top_p", json!(1.1), "top_p"),
        (
            "input",
            json!([{"type":null,"role":"user","content":"hi"}]),
            "type",
        ),
        (
            "input",
            json!([{"type":"function_call","name":"weather","arguments":"{}"}]),
            "function_call requires call_id",
        ),
        (
            "input",
            json!([{"type":"function_call","call_id":"a","arguments":"{}"}]),
            "function_call requires name",
        ),
        (
            "input",
            json!([{"type":"function_call","call_id":"a","name":"weather","arguments":{}}]),
            "function_call requires arguments text",
        ),
        (
            "input",
            json!([{"type":"function_call_output","output":"hi"}]),
            "function_call_output requires call_id",
        ),
        (
            "input",
            json!([{"type":"function_call_output","call_id":"a","output":{}}]),
            "function_call_output requires output text",
        ),
        ("reasoning", json!(3), "reasoning"),
        ("text", json!(3), "text"),
        ("text", json!({"verbosity":"high"}), "text"),
        ("text", json!({"format":{"type":"xml"}}), "text.format"),
        (
            "input",
            json!([{"content":"hi"}]),
            "input message role is required",
        ),
        ("input", json!([{"role":"user","content":3}]), "content"),
        (
            "input",
            json!([{"type":"function_call","call_id":"","name":"weather","arguments":"{}"}]),
            "function_call requires call_id",
        ),
        (
            "input",
            json!([{"type":"function_call","call_id":"a","name":"","arguments":"{}"}]),
            "function_call requires name",
        ),
        (
            "input",
            json!([{"type":"function_call_output","call_id":"","output":"hi"}]),
            "function_call_output requires call_id",
        ),
        (
            "tools",
            json!([{"type":"function","name":"","parameters":{}}]),
            "function tool requires name and parameters",
        ),
        (
            "tool_choice",
            json!({"type":"function","name":""}),
            "unsupported tool_choice",
        ),
        ("background", json!(true), "background"),
    ] {
        let mut request = json!({"model":"local/test","input":"hello"});
        request[field] = value;
        let (status, error) = post(app.clone(), request).await;
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

#[tokio::test]
async fn images_schema_and_effort_reach_upstream() {
    let mut server = mockito::Server::new_async().await;
    let schema = json!({"type":"object","properties":{"ok":{"type":"boolean"}},"required":["ok"],"additionalProperties":false});
    let upstream = server.mock("POST","/responses").match_body(mockito::Matcher::PartialJson(json!({
        "instructions":"Inspect","input":[{"role":"user","content":[{"type":"input_text","text":"Image?"},{"type":"input_image","image_url":"https://example.com/image.png","detail":"low"}]}],
        "reasoning":{"effort":"low"},"text":{"format":{"type":"json_schema","name":"answer","strict":true,"schema":schema}}
    }))).with_body(json!({"id":"resp_schema","status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"{\"ok\":true}"}]}],"usage":{}}).to_string()).expect(1).create_async().await;
    let app = llmshim::proxy::app(
        Router::new().register(
            "openai",
            Box::new(OpenAi::new("key".into()).with_base_url(server.url())),
        ),
        None,
    );
    let mut request = json!({"model":"openai/gpt-6-astra","input":[{"role":"developer","content":"Inspect"},{"role":"user","content":[{"type":"input_text","text":"Image?"},{"type":"input_image","image_url":"https://example.com/image.png","detail":"low"}]}],"reasoning":{"effort":"low"},"text":{"format":{"type":"json_schema","name":"answer","strict":true,"schema":schema}}});
    let (status, response) = post(app.clone(), request.clone()).await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(response["output"][0]["content"][0]["text"], "{\"ok\":true}");
    request["input"][1]["content"][1]["image_url"] = Value::Null;
    let (status, error) = post(app, request).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(error["error"]["message"]
        .as_str()
        .unwrap()
        .contains("image_url"));
    upstream.assert_async().await;
}

#[tokio::test]
async fn summary_and_usage_keep_provider_values() {
    let mut server = mockito::Server::new_async().await;
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/responses-api-reference.json")).unwrap();
    let mut response = fixture.clone();
    response["output"].as_array_mut().unwrap().insert(0,json!({"type":"reasoning","id":"rs_test","summary":[{"type":"summary_text","text":"A concise summary"}]}));
    let upstream = server
        .mock("POST", "/responses")
        .with_body(response.to_string())
        .expect(1)
        .create_async()
        .await;
    let app = llmshim::proxy::app(
        Router::new().register(
            "openai",
            Box::new(OpenAi::new("key".into()).with_base_url(server.url())),
        ),
        None,
    );
    let (status, response) = post(app, json!({"model":"openai/test","input":"hi"})).await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(response["output"][0]["type"], "reasoning");
    assert_eq!(
        response["output"][0]["summary"][0]["text"],
        "A concise summary"
    );
    assert_eq!(
        response["output"][1]["content"][0]["text"],
        fixture["output"][0]["content"][0]["text"]
    );
    assert_eq!(
        response["usage"]["input_tokens"],
        fixture["usage"]["input_tokens"]
    );
    assert_eq!(
        response["usage"]["output_tokens_details"],
        fixture["usage"]["output_tokens_details"]
    );
    assert_eq!(
        response["usage"]["input_tokens_details"]["cached_tokens"],
        fixture["usage"]["input_tokens_details"]["cached_tokens"]
    );
    upstream.assert_async().await;
}

#[tokio::test]
async fn incomplete_refusal_and_empty_output_are_distinct() {
    for (finish, message, expected) in [
        (
            "length",
            json!({"role":"assistant","content":"Partial","tool_calls":[{"id":"truncated","type":"function","function":{"name":"weather","arguments":"{}"}}]}),
            "incomplete",
        ),
        (
            "content_filter",
            json!({"role":"assistant","content":null,"refusal":"Cannot answer"}),
            "completed",
        ),
        (
            "stop",
            json!({"role":"assistant","content":""}),
            "completed",
        ),
    ] {
        let mut server = mockito::Server::new_async().await;
        let upstream = server.mock("POST","/chat/completions").with_body(json!({"id":"chat_test","choices":[{"message":message,"finish_reason":finish}],"usage":{}}).to_string()).expect(1).create_async().await;
        let app = llmshim::proxy::app(
            Router::new().register(
                "local",
                Box::new(OpenAiCompatible::new("local", server.url(), None)),
            ),
            None,
        );
        let (status, response) = post(app, json!({"model":"local/test","input":"hi"})).await;
        assert_eq!(status, StatusCode::OK, "{response}");
        assert_eq!(response["status"], expected);
        if finish == "length" {
            assert_eq!(
                response["incomplete_details"]["reason"],
                "max_output_tokens"
            );
            assert_eq!(response["output"][0]["status"], "incomplete");
            assert_eq!(response["output"][1]["status"], "incomplete");
            assert_eq!(response["output"][1]["arguments"], "{}");
        } else if finish == "content_filter" {
            assert!(response["incomplete_details"].is_null());
            assert_eq!(
                response["output"][0]["content"][0],
                json!({"type":"refusal","refusal":"Cannot answer"})
            );
        } else {
            assert_eq!(response["output"], json!([]));
        }
        upstream.assert_async().await;
    }
}

#[tokio::test]
async fn parallel_calls_remain_one_turn_and_owned_ids_require_receipts() {
    let mut server = mockito::Server::new_async().await;
    let upstream = server.mock("POST","/chat/completions").match_body(mockito::Matcher::PartialJson(json!({"messages":[
        {"role":"assistant","content":null,"tool_calls":[
            {"id":"a","type":"function","function":{"name":"weather","arguments":"{}"}},
            {"id":"b","type":"function","function":{"name":"weather","arguments":"{}"}}]},
        {"role":"tool","tool_call_id":"a","content":"sunny"},
        {"role":"tool","tool_call_id":"b","content":"rainy"}
    ]}))).with_body(json!({"id":"chat_test","choices":[{"message":{"role":"assistant","content":"Mixed"},"finish_reason":"stop"}],"usage":{}}).to_string()).expect(1).create_async().await;
    let app = llmshim::proxy::app(
        Router::new().register(
            "local",
            Box::new(OpenAiCompatible::new("local", server.url(), None)),
        ),
        None,
    );
    let mut request = json!({"model":"local/test","input":[
        {"type":"function_call","call_id":"a","name":"weather","arguments":"{}"},
        {"type":"function_call","call_id":"b","name":"weather","arguments":"{}"},
        {"type":"function_call_output","call_id":"a","output":"sunny"},
        {"type":"function_call_output","call_id":"b","output":"rainy"}
    ]});
    let (status, response) = post(app.clone(), request.clone()).await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(response["output"][0]["content"][0]["text"], "Mixed");
    request["input"][0]["call_id"] = json!("call_ls_unissued");
    request["input"][2]["call_id"] = json!("call_ls_unissued");
    let (status, error) = post(app, request).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(error["error"]["message"]
        .as_str()
        .unwrap()
        .contains("receipt"));
    upstream.assert_async().await;
}

#[tokio::test]
async fn missing_upstream_ids_do_not_merge_distinct_responses() {
    let mut server = mockito::Server::new_async().await;
    let upstream = server.mock("POST","/chat/completions").with_body(json!({"choices":[{"message":{"role":"assistant","content":"Hello"},"finish_reason":"stop"}],"usage":{}}).to_string()).expect(2).create_async().await;
    let app = llmshim::proxy::app(
        Router::new().register(
            "local",
            Box::new(OpenAiCompatible::new("local", server.url(), None)),
        ),
        None,
    );
    let request = json!({"model":"local/test","input":"hi"});
    let (status, first) = post(app.clone(), request.clone()).await;
    assert_eq!(status, StatusCode::OK);
    let (status, second) = post(app, request).await;
    assert_eq!(status, StatusCode::OK);
    assert_ne!(first["id"], second["id"]);
    assert_ne!(first["output"][0]["id"], second["output"][0]["id"]);
    upstream.assert_async().await;
}
