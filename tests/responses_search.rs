#![cfg(feature = "proxy")]

use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use llmshim::{providers::openai_compat::OpenAiCompatible, reasoning::WireFormat, router::Router};
use serde_json::{json, Value};
use tower::ServiceExt;

#[tokio::test]
async fn native_search_items_survive_json_sse_and_replay() {
    for streaming in [false, true] {
        let mut server = mockito::Server::new_async().await;
        let call = json!({"type": "tool_search_call", "id": "search_item",
            "call_id": "search_call", "status": "completed", "execution": "client",
            "arguments": {"query": "read"}});
        let search_output = json!({"type": "tool_search_output", "id": "search_result",
        "call_id": "search_call", "execution": "client", "status": "completed",
        "tools": [{"type": "namespace", "name": "discovered", "tools": [
            {"type": "custom", "name": "apply_patch", "format": {"type": "text"}},
        ]}]});
        let search = json!({"type": "tool_search", "execution": "client",
            "description": "Find a tool", "parameters": {"type": "object", "properties": {
                "query": {"type": "string"}}, "required": ["query"]}});
        let patch = "*** Begin Patch\n*** End Patch\n";
        let custom = json!({"type": "custom_tool_call", "id": "patch_item",
            "call_id": "patch_call", "name": "apply_patch", "namespace": "discovered",
            "input": patch, "status": "completed"});
        let message = json!({"type": "message", "id": "text_item", "role": "assistant",
            "status": "completed", "content": [{"type": "output_text", "text": "ready"}]});
        let response = json!({"id": "response", "status": "completed",
            "output": [call, search_output, custom, message]});
        let first_body = if streaming {
            format!(
                "data: {}\n\ndata: {}\n\n",
                json!({"type": "response.output_text.delta", "output_index": 3,
                    "item_id": "text_item", "delta": "ready"}),
                json!({"type": "response.completed", "response": response})
            )
        } else {
            response.to_string()
        };
        let first = server
            .mock("POST", "/responses")
            .match_body(mockito::Matcher::PartialJson(json!({"tools": [search]})))
            .with_header(
                "content-type",
                if streaming {
                    "text/event-stream"
                } else {
                    "application/json"
                },
            )
            .with_body(first_body)
            .expect(1)
            .create_async()
            .await;
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
        let request = json!({"model": "local/test", "input": "hello",
            "tools": [search], "stream": streaming});
        let result = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/responses")
                    .header("content-type", "application/json")
                    .body(Body::from(request.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(result.status(), StatusCode::OK);
        let body = to_bytes(result.into_body(), 100_000).await.unwrap();
        let result: Value = if streaming {
            let text = std::str::from_utf8(&body).unwrap();
            let events: Vec<Value> = text
                .split("\n\n")
                .filter_map(|frame| {
                    let data = frame.lines().find_map(|line| line.strip_prefix("data: "))?;
                    serde_json::from_str(data).ok()
                })
                .collect();
            assert!(events
                .iter()
                .any(|event| event["type"] == "response.output_item.done"
                    && event["item"]["type"] == "tool_search_call"));
            let indices: Vec<Value> = events
                .iter()
                .filter(|event| event["type"] == "response.output_item.done")
                .map(|event| event["output_index"].clone())
                .collect();
            assert_eq!(indices, json!([0, 1, 2, 3]).as_array().unwrap().clone());
            let text = events
                .iter()
                .find(|event| event["type"] == "response.output_text.delta")
                .unwrap();
            assert_eq!(text["output_index"], 3);
            assert_eq!(text["delta"], "ready");
            events.last().unwrap()["response"].clone()
        } else {
            serde_json::from_slice(&body).unwrap()
        };
        assert_eq!(result["output"][0], call);
        assert_eq!(result["output"][1], search_output);
        assert_eq!(result["output"][2]["type"], "custom_tool_call");
        assert_eq!(result["output"][2]["name"], "apply_patch");
        assert_eq!(result["output"][2]["namespace"], "discovered");
        assert_eq!(result["output"][2]["input"], patch);
        assert_eq!(result["output"][3]["type"], "message");
        assert_eq!(result["output"][3]["content"][0]["text"], "ready");
        first.assert_async().await;
        first.remove_async().await;
        let expected_search = search.clone();
        let second = server
            .mock("POST", "/responses")
            .match_request(move |request| {
                let body: Value = serde_json::from_slice(request.body().unwrap()).unwrap();
                let items = body["input"].as_array().unwrap();
                body["tools"] == json!([expected_search])
                    && items[1] == call
                    && items[2] == search_output
                    && items[3]["type"] == "custom_tool_call"
                    && items[3]["namespace"] == "discovered"
                    && items[3]["input"] == patch
                    && items[4]["content"][0]["text"] == "ready"
                    && items[5]["type"] == "custom_tool_call_output"
            })
            .with_body(
                json!({"id": "second", "status": "completed", "output": [{
                    "type": "custom_tool_call", "id": "second_patch", "call_id": "second_call",
                    "name": "apply_patch", "namespace": "discovered", "input": patch,
                }]})
                .to_string(),
            )
            .expect(1)
            .create_async()
            .await;
        let mut input = vec![json!({"role": "user", "content": "hello"})];
        input.extend(result["output"].as_array().unwrap().iter().cloned());
        input.push(json!({"type": "custom_tool_call_output",
            "call_id": result["output"][2]["call_id"], "output": "ok"}));
        let request = json!({"model": "local/test", "input": input, "tools": [search]});
        let result = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/responses")
                    .header("content-type", "application/json")
                    .body(Body::from(request.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = result.status();
        let body = to_bytes(result.into_body(), 100_000).await.unwrap();
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        let next: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(next["output"][0]["type"], "custom_tool_call");
        assert_eq!(next["output"][0]["name"], "apply_patch");
        assert_eq!(next["output"][0]["namespace"], "discovered");
        assert_eq!(next["output"][0]["input"], patch);
        second.assert_async().await;
    }
}
