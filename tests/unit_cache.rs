use llmshim::{
    cache,
    provider::{Provider, ProviderRequest},
    providers::{
        anthropic::Anthropic, gemini::Gemini, openai::OpenAi, openai_compat::OpenAiCompatible,
        openrouter::OpenRouter, xai::Xai,
    },
};
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
fn eligible_boundaries_are_placed_from_the_end_and_capped_at_four() {
    let messages: Vec<_> = (0..7)
        .map(|i| json!({"role":"user","content":format!("part{i}")}))
        .collect();
    let segments:Vec<_>=(0..7).map(|i|json!({"upto_message":i,"stability":if i==6{"turn"}else if i<3{"static"}else{"session"}})).collect();
    let request = json!({"messages":messages,"x-cache":{"segments":segments}});
    let before = request.clone();
    let r = Anthropic::new("test".into())
        .transform_request("claude-sonnet-4-6", &request)
        .unwrap();
    assert_eq!(markers(&r.body).len(), 4);
    assert!(r.body["messages"][0]["content"].is_string());
    assert!(r.body["messages"][1]["content"].is_string());
    assert_eq!(
        r.body["messages"][2]["content"][0]["cache_control"]["ttl"],
        "1h"
    );
    assert_eq!(
        r.body["messages"][5]["content"][0]["cache_control"]["ttl"],
        "5m"
    );
    assert!(r.body["messages"][6]["content"].is_string());
    assert!(r
        .headers
        .iter()
        .any(|(k, v)| k == "anthropic-beta" && v.contains("extended-cache-ttl")));
    assert_eq!(request, before);
    assert!(r.body.get("x-cache").is_none());
}

#[test]
fn system_tools_and_tool_results_keep_correct_end_markers() {
    let request = json!({"messages":[{"role":"system","content":"system"},{"role":"user","content":"call"},{"role":"assistant","tool_calls":[{"id":"c","function":{"name":"read","arguments":"{}"}}]},{"role":"tool","tool_call_id":"c","content":"result"}],"x-cache":{"segments":[{"upto_message":0,"stability":"static"},{"upto_message":2,"stability":"session"},{"upto_message":3,"stability":"session"}]}});
    let r = Anthropic::new("test".into())
        .transform_request("claude-sonnet-4-6", &request)
        .unwrap();
    assert_eq!(r.body["system"][0]["cache_control"]["ttl"], "1h");
    assert_eq!(r.body["messages"][1]["content"][0]["type"], "tool_use");
    assert_eq!(
        r.body["messages"][1]["content"][0]["cache_control"]["ttl"],
        "5m"
    );
    assert_eq!(r.body["messages"][2]["content"][0]["type"], "tool_result");
    assert_eq!(
        r.body["messages"][2]["content"][0]["cache_control"]["ttl"],
        "5m"
    );
}

#[test]
fn manual_markers_consume_slots_and_absence_keeps_passthrough() {
    let p = Anthropic::new("test".into());
    let base = json!({"messages":[{"role":"user","content":"hi"}],"tools":[{"type":"function","function":{"name":"read","parameters":{"type":"object","properties":{}}},"cache_control":{"type":"ephemeral","ttl":"1h"}}]});
    let r = p.transform_request("claude-sonnet-4-6", &base).unwrap();
    assert_eq!(markers(&r.body).len(), 1);
    assert!(r.body["messages"][0]["content"].is_string());
    let mut annotated = base.clone();
    annotated["x-cache"] = json!({"segments":[{"upto_message":0,"stability":"session"}]});
    assert_eq!(
        markers(
            &p.transform_request("claude-sonnet-4-6", &annotated)
                .unwrap()
                .body
        )
        .len(),
        2
    );
    let plain = p
        .transform_request(
            "claude-sonnet-4-6",
            &json!({"messages":[{"role":"user","content":"hi"}]}),
        )
        .unwrap();
    assert!(markers(&plain.body).is_empty());
}

#[test]
fn responses_get_only_the_explicit_affinity_key() {
    let request = json!({"messages":[{"role":"user","content":"hi"}],"x-cache":{"key":"session:branch","segments":[{"upto_message":0,"stability":"static"}]}});
    for p in [
        Box::new(OpenAi::new("test".into())) as Box<dyn Provider>,
        Box::new(Xai::new("test".into())),
    ] {
        let body = p.transform_request("model", &request).unwrap().body;
        assert_eq!(body["prompt_cache_key"], "session:branch");
        assert!(body.get("x-cache").is_none());
        assert!(markers(&body).is_empty());
        assert!(p
            .transform_request("model", &json!({"messages":[]}))
            .unwrap()
            .body
            .get("prompt_cache_key")
            .is_none());
    }
    for p in [
        Box::new(Gemini::new("test".into())) as Box<dyn Provider>,
        Box::new(OpenRouter::new("test".into())),
        Box::new(OpenAiCompatible::new("custom", "", None)),
    ] {
        let body = p.transform_request("model", &request).unwrap().body;
        assert!(body.get("x-cache").is_none());
        assert!(body.get("prompt_cache_key").is_none());
        assert!(markers(&body).is_empty());
    }
}

#[test]
fn literal_schema_fields_are_not_cache_breakpoints() {
    let p = Anthropic::new("test".into());
    let r=p.transform_request("claude-sonnet-4-6",&json!({"messages":[],"tools":[{"type":"function","function":{"name":"f","parameters":{"type":"object","default":{"cache_control":{"type":"ephemeral","ttl":"1h"}}}}}]})).unwrap();
    assert!(!r
        .headers
        .iter()
        .any(|(k, v)| k == "anthropic-beta" && v.contains("extended-cache-ttl")));
}

#[test]
fn continuation_checks_cover_settings_prefix_endpoint_and_credentials() {
    let previous = json!({"model":"m","input":[{"role":"user","content":"first"}],"include":["reasoning.encrypted_content"],"store":false,"reasoning":{"effort":"low"}});
    let mut next = previous.clone();
    next["input"]
        .as_array_mut()
        .unwrap()
        .push(json!({"role":"assistant","content":"answer"}));
    assert!(cache::continuation_matches(&previous, &next));
    for (key, value) in [
        ("include", json!([])),
        ("store", json!(true)),
        ("reasoning", json!({"effort":"high"})),
    ] {
        let mut changed = next.clone();
        changed[key] = value;
        assert!(!cache::continuation_matches(&previous, &changed));
    }
    let mut changed = next.clone();
    changed["input"][0]["content"] = json!("changed");
    assert!(!cache::continuation_matches(&previous, &changed));
    let a = ProviderRequest {
        url: "https://one.test".into(),
        headers: vec![("authorization".into(), "a".into())],
        body: previous,
    };
    let mut b = ProviderRequest {
        url: a.url.clone(),
        headers: a.headers.clone(),
        body: next,
    };
    assert!(b.can_continue_from(&a));
    b.headers[0].1 = "b".into();
    assert!(!b.can_continue_from(&a));
}

#[test]
fn invalid_annotations_fail_locally() {
    let p = Anthropic::new("test".into());
    for policy in [
        json!({"segments":[{"upto_message":9,"stability":"static"}]}),
        json!({"segments":[{"upto_message":0,"stability":"random"}]}),
    ] {
        assert!(p
            .transform_request(
                "claude-sonnet-4-6",
                &json!({"messages":[{"role":"user","content":"hi"}],"x-cache":policy})
            )
            .is_err());
    }
}

#[test]
fn managed_segments_supersede_automatic_caching_and_validate_native_ttl_order() {
    let p = Anthropic::new("test".into());
    let req = json!({"messages":[{"role":"user","content":"volatile"}],"cache_control":{"type":"ephemeral"},"x-cache":{"segments":[{"upto_message":0,"stability":"turn"}]}});
    assert!(markers(&p.transform_request("claude-sonnet-4-6", &req).unwrap().body).is_empty());
    let req = json!({"messages":[{"role":"user","content":"session"},{"role":"user","content":"static"}],"x-cache":{"segments":[{"upto_message":0,"stability":"session"},{"upto_message":1,"stability":"static"}]}});
    assert!(p.transform_request("claude-sonnet-4-6", &req).is_err());
}

#[cfg(feature = "proxy")]
#[tokio::test]
async fn proxy_preserves_cache_annotations_through_actual_dispatch() {
    use axum::{
        body::{to_bytes, Body},
        http::Request,
    };
    use tower::ServiceExt;
    let mut server = mockito::Server::new_async().await;
    let expected = json!({"system":[{"type":"text","text":"stable","cache_control":{"type":"ephemeral","ttl":"1h"}}]});
    let mock=server.mock("POST","/messages").match_body(mockito::Matcher::PartialJson(expected))
        .with_body(json!({"stop_reason":"end_turn","content":[{"type":"text","text":"ok"}],"usage":{"cache_read_input_tokens":9}}).to_string()).create_async().await;
    let router = llmshim::router::Router::new().register(
        "anthropic",
        Box::new(Anthropic::new("test".into()).with_base_url(server.url())),
    );
    let app = llmshim::proxy::app(router, None);
    let req = json!({"model":"anthropic/claude-sonnet-4-6","messages":[{"role":"system","content":"stable"},{"role":"user","content":"hi"}],"x-cache":{"segments":[{"upto_message":0,"stability":"static"}]}});
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat")
                .header("content-type", "application/json")
                .body(Body::from(req.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let response: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 100_000).await.unwrap()).unwrap();
    assert_eq!(response["usage"]["cache_read_tokens"], 9);
    mock.assert_async().await;
}
