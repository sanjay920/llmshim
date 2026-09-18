use futures::StreamExt;
use llmshim::{
    catalog::{ModelCapabilities, Support},
    provider::Provider,
    providers::{
        anthropic::Anthropic, gemini::Gemini, openai::OpenAi, openai_compat::OpenAiCompatible,
        xai::Xai,
    },
    reasoning::{ReplayTarget, WireFormat},
    shim::Plan,
};
use serde_json::{json, Value};

fn request(mode: &str, schema: Value) -> Value {
    json!({"model":"local/test","messages":[{"role":"user","content":"answer"}],"response_format":{"type":"json_schema","json_schema":{"name":"answer","schema":schema}},"x-shim":{"structured_output":mode}})
}
fn response(content: Value) -> Value {
    json!({"id":"r","choices":[{"index":0,"message":{"role":"assistant","content":content},"finish_reason":"stop"}],"usage":{"prompt_tokens":7,"completion_tokens":3,"total_tokens":10,"cache_read_tokens":2,"cache_write_tokens":1}})
}
fn target() -> ReplayTarget {
    ReplayTarget::new("local", "test", WireFormat::OpenAiChat)
}
fn plan(request: &Value) -> Plan {
    Plan::with_capabilities(
        WireFormat::OpenAiChat,
        request,
        ModelCapabilities::unknown(),
    )
    .unwrap()
}
fn call_response(name: &str, args: Value) -> Value {
    let mut r = response(Value::Null);
    r["choices"][0]["message"]["tool_calls"] = json!([{"id":"hidden","type":"function","function":{"name":name,"arguments":args.to_string()}}]);
    r["choices"][0]["finish_reason"] = json!("tool_calls");
    r
}
fn tool_request() -> Value {
    json!({"model":"local/test","messages":[{"role":"user","content":"read"}],"tools":[{"type":"function","function":{"name":"read","parameters":{"type":"object","properties":{"path":{"type":"string","minLength":2}},"required":["path"],"additionalProperties":false}}}],"x-shim":{"tool_calling":"prompt"}})
}

#[test]
fn native_output_format_is_translated_on_every_wire() {
    let req = request(
        "native",
        json!({"type":"object","properties":{"answer":{"type":"integer","minimum":2}},"required":["answer"]}),
    );
    let providers: Vec<(Box<dyn Provider>, &str, &str)> = vec![
        (
            Box::new(OpenAi::new("test".into())),
            "gpt-6-astra",
            "/text/format/schema",
        ),
        (
            Box::new(Xai::new("test".into())),
            "grok-4",
            "/text/format/schema",
        ),
        (
            Box::new(Anthropic::new("test".into())),
            "claude-sonnet-4-6",
            "/output_config/format/schema",
        ),
        (
            Box::new(Gemini::new("test".into())),
            "gemini-2.5-flash",
            "/generationConfig/responseSchema",
        ),
        (
            Box::new(OpenAiCompatible::new("local", "http://localhost", None)),
            "test",
            "/response_format/json_schema/schema",
        ),
    ];
    for (provider, model, path) in providers {
        let native = provider.transform_request(model, &req).unwrap();
        assert_eq!(
            native.body.pointer(path).unwrap()["type"],
            "object",
            "{}",
            provider.name()
        );
        assert!(native.body.get("x-shim").is_none());
    }
}

#[test]
fn catalog_auto_prefers_native_then_forced_then_prompt_and_respects_forced_ban() {
    let req = request("auto", json!({"type":"object"}));
    for (caps, native, forced) in [
        (
            ModelCapabilities::unknown().with_structured_output(Support::Supported),
            true,
            false,
        ),
        (
            ModelCapabilities::unknown().with_tools(Support::Supported),
            false,
            true,
        ),
        (
            ModelCapabilities::unknown()
                .with_tools(Support::Supported)
                .with_forced_tool_choice(Support::Unsupported),
            false,
            false,
        ),
        (ModelCapabilities::unknown(), false, false),
    ] {
        let p = Plan::with_capabilities(WireFormat::AnthropicMessages, &req, caps).unwrap();
        let r = p.render().unwrap();
        assert_eq!(r.get("response_format").is_some(), native);
        assert_eq!(r["tool_choice"]["function"]["name"].is_string(), forced);
        if !native && !forced {
            assert!(r["messages"][0]["content"]
                .as_str()
                .unwrap()
                .contains("schema"));
        }
    }
}

#[test]
fn validation_uses_original_constraints_and_preserves_nullable_values() {
    let req = request(
        "native",
        json!({"type":"object","properties":{"n":{"type":"integer","minimum":3},"optional":{"type":"string"},"nullable":{"type":["string","null"]}},"required":["n"],"additionalProperties":false}),
    );
    let p = plan(&req);
    assert!(p
        .finish(&mut response(json!("{\"n\":1}")), &target())
        .is_err());
    let mut r = response(json!("{\"n\":3,\"optional\":null,\"nullable\":null}"));
    p.finish(&mut r, &target()).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(r["choices"][0]["message"]["content"].as_str().unwrap())
            .unwrap(),
        json!({"n":3,"nullable":null})
    );
    // Prompted output has no strict-schema null convention.
    let mut prompt = req.clone();
    prompt["x-shim"]["structured_output"] = json!("prompt");
    assert!(plan(&prompt)
        .finish(
            &mut response(json!("{\"n\":3,\"optional\":null}")),
            &target()
        )
        .is_err());
}

#[test]
fn scalar_array_forced_output_is_unwrapped_and_invisible() {
    for (schema, value) in [
        (json!({"type":"integer"}), json!(4)),
        (
            json!({"type":"array","items":{"type":"string"}}),
            json!(["a"]),
        ),
    ] {
        let req = request("forced_tool", schema);
        let before = req.clone();
        let p = plan(&req);
        let rendered = p.render().unwrap();
        let name = rendered["tool_choice"]["function"]["name"]
            .as_str()
            .unwrap();
        let mut r = call_response(name, json!({"response":value}));
        r["choices"][0]["message"]["reasoning"] = json!([{"text":format!("call {name}")}]);
        p.finish(&mut r, &target()).unwrap();
        assert_eq!(r["choices"][0]["message"]["content"], value.to_string());
        assert_eq!(r["choices"][0]["finish_reason"], "stop");
        assert!(!r.to_string().contains(name));
        assert!(r["choices"][0]["message"].get("tool_calls").is_none());
        assert_eq!(req, before);
    }
}

#[test]
fn prompt_calls_validate_names_arguments_choices_and_have_replayable_owned_ids() {
    let req = tool_request();
    let before = req.clone();
    let p = plan(&req);
    let mut r=response(json!(json!({"content":null,"tool_calls":[{"name":"read","arguments":{"path":"ab"}},{"name":"read","arguments":{"path":"cd"}}]}).to_string()));
    p.finish(&mut r, &target()).unwrap();
    let msg = &r["choices"][0]["message"];
    assert!(msg["tool_calls"][0]["id"]
        .as_str()
        .unwrap()
        .starts_with("call_ls_"));
    assert_ne!(msg["tool_calls"][0]["id"], msg["tool_calls"][1]["id"]);
    let mut follow = req.clone();
    follow["messages"].as_array_mut().unwrap().push(msg.clone());
    for call in msg["tool_calls"].as_array().unwrap() {
        follow["messages"]
            .as_array_mut()
            .unwrap()
            .push(json!({"role":"tool","tool_call_id":call["id"],"content":"done"}));
    }
    let text = plan(&follow).render().unwrap();
    assert!(text.get("tools").is_none());
    assert!(!text["messages"].to_string().contains("wire_ids"));
    OpenAiCompatible::new("local", "http://localhost", None)
        .transform_request("test", &text)
        .unwrap();
    // Switch to a native-capable endpoint and restore paired tool history.
    follow["x-shim"]["tool_calling"] = json!("native");
    OpenAiCompatible::new("local", "http://localhost", None)
        .transform_request("test", &follow)
        .unwrap();
    for call in [
        json!({"name":"unknown","arguments":{}}),
        json!({"name":"read","arguments":{"path":"x"}}),
    ] {
        assert!(p
            .finish(
                &mut response(json!(
                    json!({"content":null,"tool_calls":[call]}).to_string()
                )),
                &target()
            )
            .is_err());
    }
    let mut none = req.clone();
    none["tool_choice"] = json!("none");
    assert!(plan(&none).finish(&mut response(json!("{\"content\":null,\"tool_calls\":[{\"name\":\"read\",\"arguments\":{\"path\":\"ab\"}}]}")),&target()).is_err());
    let mut required = req.clone();
    required["tool_choice"] = json!("required");
    assert!(plan(&required)
        .finish(
            &mut response(json!("{\"content\":\"ok\",\"tool_calls\":[]}")),
            &target()
        )
        .is_err());
    assert_eq!(req, before);
}

#[test]
fn reasoning_capture_returns_brief_rationale_and_preserves_user_tool_name_collision() {
    let mut req = tool_request();
    req["tools"][0]["function"]["name"] = json!("think");
    req["x-shim"] = json!({"reasoning_capture":"forced_tool"});
    let p = plan(&req);
    let rendered = p.render().unwrap();
    assert_eq!(rendered["tools"][0]["function"]["name"], "think_");
    assert_eq!(
        rendered["tools"][0]["function"]["parameters"]["properties"]
            .as_object()
            .unwrap()
            .len(),
        1
    );
    let envelope = json!({"reasoning":"The requested file contains the answer.","content":null,"tool_calls":[{"name":"think","arguments":{"path":"ab"}}]});
    let mut r = call_response("think_", json!({"text":envelope.to_string()}));
    p.finish(&mut r, &target()).unwrap();
    assert_eq!(
        r["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
        "think"
    );
    assert_eq!(
        r["choices"][0]["message"]["reasoning"][0]["text"],
        envelope["reasoning"]
    );
    assert!(!r.to_string().contains("think_"));
    let caps = ModelCapabilities::unknown().with_tools(Support::Unsupported);
    let p = Plan::with_capabilities(WireFormat::OpenAiChat, &req, caps).unwrap();
    assert!(p.render().unwrap().get("tools").is_none());
    p.finish(&mut response(json!(envelope.to_string())), &target())
        .unwrap();
}

#[test]
fn unsupported_reasoning_and_extension_overrides_cannot_bypass_plan() {
    let mut req = tool_request();
    req["reasoning_effort"] = json!("high");
    req["x-local"] = json!({"reasoning":{"effort":"high"},"tool_choice":"required","tools":[{"bad":true}],"messages":[],"x-shim":{"tool_calling":"native"}});
    let p = Plan::with_capabilities(
        WireFormat::OpenAiChat,
        &req,
        ModelCapabilities::unknown().with_reasoning(Support::Unsupported),
    )
    .unwrap();
    let rendered = p.render().unwrap();
    let native = OpenAiCompatible::new("local", "http://localhost", None)
        .transform_request("test", &rendered)
        .unwrap();
    assert!(native.body.get("tools").is_none());
    assert!(native.body.get("reasoning_effort").is_none());
    assert!(native.body.get("reasoning").is_none());
    assert!(native.body.get("x-shim").is_none());
}

#[test]
fn prompt_instructions_keep_cache_boundaries_on_original_messages() {
    let mut req = request("prompt", json!({"type":"integer"}));
    req["x-cache"] = json!({"segments":[{"upto_message":0,"stability":"session"}]});
    let rendered = plan(&req).render().unwrap();
    assert_eq!(rendered["x-cache"]["segments"][0]["upto_message"], 1);
    assert_eq!(rendered["messages"][1], req["messages"][0]);
}

#[test]
fn schema_validation_rejects_external_refs_and_honors_original_semantics() {
    for schema in [
        json!({"$ref":"https://example.invalid/never-fetch"}),
        json!({"$ref":"file:///etc/passwd"}),
        json!({"type":"bogus"}),
    ] {
        assert!(Plan::with_capabilities(
            WireFormat::OpenAiChat,
            &request("prompt", schema),
            ModelCapabilities::unknown()
        )
        .is_err());
    }
    let schema = json!({"type":"object","properties":{"n":{"type":"integer"}},"if":{"properties":{"n":{"minimum":2}}},"then":{"required":["label"]}});
    assert!(plan(&request("prompt", schema))
        .finish(&mut response(json!("{\"n\":2}")), &target())
        .is_err());
    assert!(
        llmshim::schema::validate::compile(&json!({"type":"string","format":"email"}))
            .unwrap()
            .is_valid(&json!("a@example.com"))
    );
    assert!(
        !llmshim::schema::validate::compile(&json!({"type":"string","format":"email"}))
            .unwrap()
            .is_valid(&json!("bad"))
    );
}

#[tokio::test]
async fn structured_completion_repairs_once_and_counts_both_attempts() {
    let mut server = mockito::Server::new_async().await;
    let invalid=server.mock("POST","/chat/completions").match_body(mockito::Matcher::PartialJson(json!({"messages":[{"role":"system","content":"Return only a JSON value matching this schema, without markdown fences: {\"minimum\":3,\"type\":\"integer\"}"},{"role":"user","content":"answer"}]}))).with_body(response(json!("1")).to_string()).expect(1).create_async().await;
    let valid = server
        .mock("POST", "/chat/completions")
        .match_body(mockito::Matcher::Regex("previous attempt".into()))
        .with_body(response(json!("3")).to_string())
        .expect(1)
        .create_async()
        .await;
    let provider = OpenAiCompatible::new("local", &server.url(), None);
    let mut req = request("prompt", json!({"type":"integer","minimum":3}));
    req["stream"] = json!(true);
    let before = req.clone();
    let r = llmshim::client::ShimClient::new()
        .completion(&provider, "test", &req)
        .await
        .unwrap();
    assert_eq!(r["choices"][0]["message"]["content"], "3");
    assert_eq!(r["usage"]["total_tokens"], 20);
    assert_eq!(r["usage"]["cache_read_tokens"], 4);
    assert_eq!(r["usage"]["cache_write_tokens"], 2);
    assert_eq!(req, before);
    invalid.assert_async().await;
    valid.assert_async().await;
}

#[tokio::test]
async fn repair_limit_and_refusal_without_repair() {
    let mut server = mockito::Server::new_async().await;
    let mock = server
        .mock("POST", "/chat/completions")
        .with_body(response(json!("no json")).to_string())
        .expect(2)
        .create_async()
        .await;
    let provider = OpenAiCompatible::new("local", &server.url(), None);
    let client = llmshim::client::ShimClient::new();
    let req = request("prompt", json!({"type":"integer"}));
    let error = client
        .completion(&provider, "test", &req)
        .await
        .unwrap_err()
        .to_string();
    assert!(!error.contains("no json"));
    mock.assert_async().await;
    mock.remove_async().await;
    let mut refusal = response(Value::Null);
    refusal["choices"][0]["message"]["refusal"] = json!("Cannot comply");
    let mock = server
        .mock("POST", "/chat/completions")
        .with_body(refusal.to_string())
        .expect(1)
        .create_async()
        .await;
    let r = client.completion(&provider, "test", &req).await.unwrap();
    assert_eq!(r["choices"][0]["message"]["refusal"], "Cannot comply");
    mock.assert_async().await;
}

fn events(content: &str) -> String {
    let mut s = format!(
        "data: {}\n\n",
        json!({"id":"r","choices":[{"index":0,"delta":{"content":content},"finish_reason":null}]})
    );
    s.push_str(&format!(
        "data: {}\n\n",
        json!({"id":"r","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})
    ));
    s.push_str(&format!("data: {}\n\ndata: [DONE]\n\n",json!({"choices":[],"usage":{"prompt_tokens":7,"completion_tokens":3,"total_tokens":10,"prompt_tokens_details":{"cached_tokens":2}}})));
    s
}
#[tokio::test]
async fn stream_buffers_invalid_attempt_and_emits_only_valid_output_with_total_usage() {
    let mut server = mockito::Server::new_async().await;
    let a = server
        .mock("POST", "/chat/completions")
        .match_body(mockito::Matcher::PartialJson(json!({"messages":[{"role":"system","content":"Return only a JSON value matching this schema, without markdown fences: {\"type\":\"integer\"}"},{"role":"user","content":"answer"}]})))
        .with_header("content-type", "text/event-stream")
        .with_body(events("not valid"))
        .expect(1)
        .create_async()
        .await;
    let b = server
        .mock("POST", "/chat/completions")
        .match_body(mockito::Matcher::Regex("previous attempt".into()))
        .with_header("content-type", "text/event-stream")
        .with_body(events("4"))
        .expect(1)
        .create_async()
        .await;
    let router = llmshim::router::Router::new().register(
        "local",
        Box::new(OpenAiCompatible::new("local", &server.url(), None)),
    );
    let req = request("prompt", json!({"type":"integer"}));
    let mut stream = llmshim::stream(&router, &req).await.unwrap();
    let mut chunks = Vec::new();
    while let Some(chunk) = stream.next().await {
        chunks.push(serde_json::from_str::<Value>(&chunk.unwrap()).unwrap());
    }
    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0]["choices"][0]["delta"]["content"], "4");
    assert_eq!(chunks[0]["usage"]["total_tokens"], 20);
    assert!(!json!(chunks).to_string().contains("not valid"));
    a.assert_async().await;
    b.assert_async().await;
}

#[test]
fn pinned_capture_and_structured_output_work_together() {
    let mut req = request("native", json!({"type":"array","items":{"type":"integer"}}));
    req["x-shim"]["reasoning_capture"] = json!("forced_tool");
    let p = plan(&req);
    let rendered = p.render().unwrap();
    assert!(rendered.get("response_format").is_none());
    let mut r = call_response(
        "think",
        json!({"text":json!({"reasoning":"These are the two matching records.","content":[1,2],"tool_calls":[]}).to_string()}),
    );
    p.finish(&mut r, &target()).unwrap();
    assert_eq!(r["choices"][0]["message"]["content"], "[1,2]");
}

#[test]
fn invalid_modes_fail_before_dispatch_and_fable_auto_avoids_forced_choice() {
    for config in [
        json!({"typo":true}),
        json!({"tool_calling":"forced_tool"}),
        json!(null),
    ] {
        let mut req = request("prompt", json!({"type":"object"}));
        req["x-shim"] = config;
        assert!(Plan::with_capabilities(
            WireFormat::OpenAiChat,
            &req,
            ModelCapabilities::unknown()
        )
        .is_err());
    }
    let req = request("auto", json!({"type":"object"}));
    let p = Plan::new(
        "anthropic",
        "claude-fable-5-1",
        WireFormat::AnthropicMessages,
        &req,
    )
    .unwrap();
    let native = Anthropic::new("test".into())
        .transform_request("claude-fable-5-1", &p.render().unwrap())
        .unwrap();
    assert!(native.body.get("tool_choice").is_none());
}

#[tokio::test]
async fn responses_refusals_and_incomplete_outputs_never_trigger_repair() {
    let mut server = mockito::Server::new_async().await;
    let req = request("native", json!({"type":"integer"}));
    let mock=server.mock("POST","/responses").with_body(json!({"status":"completed","output":[{"type":"message","content":[{"type":"refusal","refusal":"Cannot comply"}]}]}).to_string()).expect(1).create_async().await;
    let provider = OpenAi::new("test".into()).with_base_url(server.url());
    let r = llmshim::client::ShimClient::new()
        .completion(&provider, "gpt-6-astra", &req)
        .await
        .unwrap();
    assert_eq!(r["choices"][0]["message"]["refusal"], "Cannot comply");
    assert_eq!(r["choices"][0]["finish_reason"], "content_filter");
    mock.assert_async().await;
    mock.remove_async().await;
    let mock=server.mock("POST","/responses").with_body(json!({"status":"incomplete","output":[{"type":"message","content":[{"type":"output_text","text":"{"}]}]}).to_string()).expect(1).create_async().await;
    assert!(llmshim::client::ShimClient::new()
        .completion(&provider, "gpt-6-astra", &req)
        .await
        .is_err());
    mock.assert_async().await;
}

#[test]
fn refusal_from_terminal_snapshot_is_preserved_without_duplicate_deltas() {
    let origin = ReplayTarget::new("openai", "gpt-6-astra", WireFormat::OpenAiResponses);
    for with_delta in [false, true] {
        let mut stream = llmshim::streaming::StreamNormalizer::new(origin.clone());
        if with_delta {
            let chunk = stream
                .push(&json!({"type":"response.refusal.delta","delta":"Cannot comply"}).to_string())
                .unwrap()
                .unwrap();
            assert!(chunk.contains("Cannot comply"));
        }
        let chunk=stream.push(&json!({"type":"response.completed","response":{"status":"completed","output":[{"type":"message","content":[{"type":"refusal","refusal":"Cannot comply"}]}]}}).to_string()).unwrap().unwrap();
        let chunk: Value = serde_json::from_str(&chunk).unwrap();
        assert_eq!(chunk["choices"][0]["finish_reason"], "content_filter");
        assert_eq!(
            chunk["choices"][0]["delta"]["refusal"].is_string(),
            !with_delta
        );
    }
}

#[test]
fn json_object_requests_get_instance_validation_and_schema_fallback_uses_prompt() {
    let req = json!({"messages":[{"role":"user","content":"answer"}],"response_format":{"type":"json_object"}});
    let p = plan(&req);
    assert!(p.finish(&mut response(json!("[1]")), &target()).is_err());
    p.finish(&mut response(json!("{\"x\":1}")), &target())
        .unwrap();
    let req = request(
        "auto",
        json!({"type":"object","properties":{"next":{"$ref":"#"}}}),
    );
    let p = Plan::with_capabilities(
        WireFormat::OpenAiChat,
        &req,
        ModelCapabilities::unknown().with_structured_output(Support::Supported),
    )
    .unwrap();
    assert!(p.render().unwrap().get("response_format").is_none());
}
