use llmshim::{
    provider::Provider,
    providers::{
        anthropic::Anthropic, gemini::Gemini, openai::OpenAi, openai_compat::OpenAiCompatible,
    },
    reasoning::WireFormat,
    toolcall::{validate_history, ToolCallMap, WireToolId},
};
use serde_json::{json, Value};

fn responses() -> Value {
    json!({"id":"response_1","status":"completed","output":[{"type":"function_call","id":"fc_item","call_id":"native_correlation","name":"read","arguments":"{\"path\":\"a\"}"}]})
}
fn followup(message: Value) -> Value {
    let results: Vec<_> = message["tool_calls"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| json!({"role":"tool","tool_call_id":c["id"],"content":"done"}))
        .collect();
    let mut messages = vec![json!({"role":"user","content":"read"}), message];
    messages.extend(results);
    json!({"messages":messages})
}
fn calls(chunks: Vec<Value>) -> Vec<Value> {
    chunks
        .into_iter()
        .flat_map(|c| c["choices"].as_array().cloned().unwrap_or_default())
        .flat_map(|c| {
            c["delta"]["tool_calls"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .collect()
}

#[test]
fn responses_item_id_and_call_id_remain_distinct_and_owned_id_round_trips() {
    let p = OpenAi::new("test".into());
    let result = p.transform_response("gpt-6-astra", responses()).unwrap();
    let message = result["choices"][0]["message"].clone();
    let call = &message["tool_calls"][0];
    let id = call["id"].as_str().unwrap();
    assert!(id.starts_with("call_ls_"));
    assert_ne!(id, "native_correlation");
    assert_ne!(id, "fc_item");
    let binding: WireToolId = serde_json::from_value(call["wire_ids"][0].clone()).unwrap();
    assert_eq!(binding.id.as_deref(), Some("native_correlation"));
    assert_eq!(binding.item_id.as_deref(), Some("fc_item"));
    let mut map = ToolCallMap::default();
    map.insert(id, vec![binding.clone()]).unwrap();
    assert_eq!(map.internal_id(&binding), Some(id));
    let mut correlation_only = binding.clone();
    correlation_only.item_id = None;
    correlation_only.part_id = "not-the-item-id".into();
    assert_eq!(map.internal_id(&correlation_only), Some(id));
    assert_eq!(map.wire_ids(id), Some(&[binding][..]));
    let req = p
        .transform_request("gpt-6-astra", &followup(message))
        .unwrap();
    let item = req.body["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["type"] == "function_call")
        .unwrap();
    assert_eq!(item["id"], "fc_item");
    assert_eq!(item["call_id"], "native_correlation");
    let output = req.body["input"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["type"] == "function_call_output")
        .unwrap();
    assert_eq!(output["call_id"], "native_correlation");
    assert_eq!(result["choices"][0]["finish_reason"], "tool_calls");
}

#[test]
fn cross_provider_replay_maps_calls_and_results_together_without_mutating_history() {
    let source = OpenAi::new("test".into())
        .transform_response("gpt-6-astra", responses())
        .unwrap();
    let request = followup(source["choices"][0]["message"].clone());
    let original = request.clone();
    let p = Anthropic::new("test".into());
    let wire = p.transform_request("claude-sonnet-4-6", &request).unwrap();
    let id = &wire.body["messages"][1]["content"][0]["id"];
    assert_eq!(id, &request["messages"][1]["tool_calls"][0]["id"]);
    assert_eq!(&wire.body["messages"][2]["content"][0]["tool_use_id"], id);
    assert_eq!(request, original);
    assert!(!wire.body.to_string().contains("wire_ids"));
    let again = p.transform_request("claude-sonnet-4-6", &request).unwrap();
    assert_eq!(wire.body, again.body);
}

#[test]
fn signed_anthropic_prefix_uses_original_tool_ids() {
    let p = Anthropic::new("test".into());
    let content = json!([{"type":"thinking","thinking":"think","signature":"signed"},{"type":"tool_use","id":"toolu_original","name":"read","input":{"path":"a"}}]);
    let response = p
        .transform_response(
            "claude-sonnet-4-6",
            json!({"id":"a1","content":content,"stop_reason":"tool_use"}),
        )
        .unwrap();
    let req = p
        .transform_request(
            "claude-sonnet-4-6",
            &followup(response["choices"][0]["message"].clone()),
        )
        .unwrap();
    assert_eq!(req.body["messages"][1]["content"], content);
    assert_eq!(
        req.body["messages"][2]["content"][0]["tool_use_id"],
        "toolu_original"
    );
}

#[test]
fn central_pairing_rejects_missing_duplicate_and_unmatched_results() {
    let call = json!({"role":"assistant","tool_calls":[{"id":"c","function":{"name":"read","arguments":"{}"}}]});
    let result = json!({"role":"tool","tool_call_id":"c","content":"ok"});
    for messages in [
        vec![call.clone()],
        vec![result.clone()],
        vec![call.clone(), result.clone(), result.clone()],
        vec![
            call.clone(),
            json!({"role":"assistant","content":"next"}),
            result.clone(),
        ],
    ] {
        assert!(validate_history(&messages).is_err());
        for p in [
            Box::new(OpenAi::new("test".into())) as Box<dyn Provider>,
            Box::new(Anthropic::new("test".into())),
            Box::new(OpenAiCompatible::new("custom", "", None)),
        ] {
            assert!(p
                .transform_request("model", &json!({"messages":messages}))
                .is_err());
        }
    }
    assert!(validate_history(&[call, result]).is_ok());
}

#[test]
fn native_override_cannot_bypass_pairing() {
    let req = json!({"messages":[],"x-openai":{"input":[{"type":"function_call","call_id":"c","name":"read","arguments":"{}"}]}});
    assert!(OpenAi::new("test".into())
        .transform_request("gpt-6-astra", &req)
        .is_err());
    let req = json!({"messages":[],"x-anthropic":{"messages":[{"role":"assistant","content":[{"type":"tool_use","id":"c","name":"read","input":{}}]}]}});
    assert!(Anthropic::new("test".into())
        .transform_request("claude-sonnet-4-6", &req)
        .is_err());
}

#[test]
fn dropping_wire_metadata_from_an_owned_call_is_an_error() {
    let p = OpenAi::new("test".into());
    let r = p.transform_response("gpt-6-astra", responses()).unwrap();
    let mut m = r["choices"][0]["message"].clone();
    m["tool_calls"][0]
        .as_object_mut()
        .unwrap()
        .remove("wire_ids");
    assert!(p.transform_request("gpt-6-astra", &followup(m)).is_err());
}

#[test]
fn gemini_optional_ids_and_parallel_signature_placement_survive() {
    let p = Gemini::new("test".into());
    for native_id in [None, Some("google_call_id")] {
        let mut fc = json!({"name":"read","args":{"path":"a"}});
        if let Some(id) = native_id {
            fc["id"] = json!(id);
        }
        let parts = json!([{"functionCall":fc,"thoughtSignature":"sig"},{"functionCall":{"name":"read","args":{"path":"b"}}}]);
        let r=p.transform_response("gemini-3.8-flash",json!({"responseId":"g1","candidates":[{"content":{"parts":parts},"finishReason":"STOP"}]})).unwrap();
        let m = r["choices"][0]["message"].clone();
        assert_ne!(m["tool_calls"][0]["id"], m["tool_calls"][1]["id"]);
        let mut req = followup(m);
        req["messages"][2]["content"] = json!("first");
        req["messages"][3]["content"] = json!("second");
        req["messages"].as_array_mut().unwrap().swap(2, 3);
        let wire = p.transform_request("gemini-3.8-flash", &req).unwrap();
        assert_eq!(wire.body["contents"][1]["parts"], parts);
        assert_eq!(
            wire.body["contents"][2]["parts"][0]["functionResponse"]["id"],
            json!(native_id)
        );
        assert_eq!(
            wire.body["contents"][2]["parts"][0]["functionResponse"]["response"]["result"],
            "first"
        );
        assert_eq!(
            wire.body["contents"][2]["parts"][1]["functionResponse"]["response"]["result"],
            "second"
        );
    }
}

#[test]
fn anthropic_parallel_fragments_are_assembled_once_with_owned_ids() {
    let p = Anthropic::new("test".into());
    let mut stream = p.stream_normalizer("claude-sonnet-4-6");
    let mut chunks = Vec::new();
    let events = [
        json!({"type":"message_start","message":{"id":"a_stream","usage":{}}}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_a","name":"read","input":{}}}),
        json!({"type":"content_block_start","index":3,"content_block":{"type":"tool_use","id":"toolu_b","name":"write","input":{}}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}),
        json!({"type":"content_block_delta","index":3,"delta":{"type":"input_json_delta","partial_json":"{\"data\":\"雪\"}"}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"a\"}"}}),
        json!({"type":"content_block_stop","index":1}),
        json!({"type":"content_block_stop","index":3}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{}}),
    ];
    for e in events {
        if let Some(c) = stream.push(&e.to_string()).unwrap() {
            chunks.push(serde_json::from_str(&c).unwrap());
        }
    }
    let tools = calls(chunks);
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0]["function"]["arguments"], "{\"path\":\"a\"}");
    assert_eq!(tools[1]["function"]["arguments"], "{\"data\":\"雪\"}");
    assert_ne!(tools[0]["id"], tools[1]["id"]);
    assert_eq!(tools[0]["wire_ids"][0]["id"], "toolu_a");
    assert!(stream.finish().unwrap().is_none());
}

#[test]
fn responses_terminal_snapshots_do_not_duplicate_or_overappend_calls() {
    let p = OpenAi::new("test".into());
    let mut stream = p.stream_normalizer("gpt-6-astra");
    let mut chunks = Vec::new();
    let item = &responses()["output"][0];
    for e in [
        json!({"type":"response.output_item.added","output_index":2,"item":{"type":"function_call","id":"fc_item","call_id":"native_correlation","name":"read","arguments":""}}),
        json!({"type":"response.function_call_arguments.delta","output_index":2,"delta":"{\"path\":\"a\"}"}),
        json!({"type":"response.output_item.done","output_index":2,"item":item}),
        json!({"type":"response.completed","response":{"id":"r1","status":"completed","output":[]}}),
    ] {
        if let Some(c) = stream.push(&e.to_string()).unwrap() {
            chunks.push(serde_json::from_str(&c).unwrap());
        }
    }
    let tools = calls(chunks);
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["function"]["arguments"], item["arguments"]);
    assert_eq!(tools[0]["wire_ids"][0]["item_id"], "fc_item");
}

#[test]
fn chat_name_id_argument_and_signature_fragments_share_one_part() {
    let p = OpenAiCompatible::new("custom", "", None);
    let mut stream = p.stream_normalizer("model");
    let mut chunks = Vec::new();
    for e in [
        json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"id":"call_","function":{"name":"re","arguments":"{\"x\":"}}]}}]}),
        json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"id":"abc","function":{"name":"ad","arguments":"1}"},"thought_signature":"sig"}]}}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
    ] {
        if let Some(c) = stream.push(&e.to_string()).unwrap() {
            chunks.push(serde_json::from_str(&c).unwrap());
        }
    }
    if let Some(c) = stream.finish().unwrap() {
        chunks.push(serde_json::from_str(&c).unwrap());
    }
    let tools = calls(chunks);
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["function"]["name"], "read");
    assert_eq!(tools[0]["wire_ids"][0]["id"], "call_abc");
    assert_eq!(tools[0]["thought_signature"]["data"], "sig");
}

#[test]
fn no_partial_json_tool_is_exposed_at_end_of_stream() {
    let p = OpenAi::new("test".into());
    let mut stream = p.stream_normalizer("gpt-6-astra");
    assert!(stream.push(&json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc","call_id":"c","name":"read","arguments":"{\"x\":"}}).to_string()).unwrap().is_none());
    assert!(stream
        .push(
            &json!({"type":"response.completed","response":{"status":"completed","output":[]}})
                .to_string()
        )
        .is_err());
    assert!(stream.is_finished());
}

#[test]
fn bidirectional_registry_distinguishes_provider_wires() {
    let mut map = ToolCallMap::default();
    let a = WireToolId {
        provider: "a".into(),
        wire: WireFormat::OpenAiChat,
        scope: "r".into(),
        part_id: "0".into(),
        id: Some("c".into()),
        item_id: None,
        signature_field: None,
    };
    let id = map.register(a.clone()).unwrap();
    assert_eq!(map.register(a.clone()).unwrap(), id);
    let mut b = a.clone();
    b.wire = WireFormat::OpenAiResponses;
    map.insert(&id, vec![b.clone()]).unwrap();
    assert_eq!(map.internal_id(&a), Some(id.as_str()));
    assert_eq!(map.internal_id(&b), Some(id.as_str()));
    assert_eq!(map.wire_ids(&id).unwrap().len(), 2);
}

#[test]
fn nested_chat_signatures_are_normalized_restored_and_filtered_without_leaks() {
    let p = OpenAiCompatible::new("google-compatible", "https://example.test", None);
    let raw = json!({"id":"r","choices":[{"message":{"role":"assistant","tool_calls":[{"id":"c","type":"function","function":{"name":"read","arguments":"{}"},"extra_content":{"google":{"thought_signature":"opaque-nested"}}}]},"finish_reason":"tool_calls"}]});
    let r = p.transform_response("gemini-3.8-flash", raw).unwrap();
    let call = &r["choices"][0]["message"]["tool_calls"][0];
    assert_eq!(call["thought_signature"]["data"], "opaque-nested");
    assert!(call.get("extra_content").is_none());
    let request = followup(r["choices"][0]["message"].clone());
    let same = p.transform_request("gemini-3.8-flash", &request).unwrap();
    assert_eq!(
        same.body["messages"][1]["tool_calls"][0]["extra_content"]["google"]["thought_signature"],
        "opaque-nested"
    );
    assert!(same.body["messages"][1]["tool_calls"][0]
        .get("thought_signature")
        .is_none());
    let other = OpenAiCompatible::new("another", "https://another.test", None)
        .transform_request("gemini-3.8-flash", &request)
        .unwrap();
    assert!(!other.body.to_string().contains("opaque-nested"));
    let legacy = followup(
        json!({"role":"assistant","tool_calls":[{"id":"old","function":{"name":"read","arguments":"{}"},"extra_content":{"google":{"thought_signature":"untracked"}}}]}),
    );
    assert!(!p
        .transform_request("gemini-3.8-flash", &legacy)
        .unwrap()
        .body
        .to_string()
        .contains("untracked"));
}

#[test]
fn gemini_late_names_and_signature_updates_use_the_original_part() {
    let p = Gemini::new("test".into());
    let mut stream = p.stream_normalizer("gemini-3.8-flash");
    for call in [
        json!({"args":{"path":"a"}}),
        json!({"name":"read","id":"g1","args":{"path":"a"}}),
    ] {
        assert!(stream
            .push(
                &json!({"candidates":[{"content":{"parts":[{"functionCall":call}]}}]}).to_string()
            )
            .unwrap()
            .is_none());
    }
    assert!(stream.push(&json!({"candidates":[{"content":{"parts":[{"functionCall":{"id":"g1"},"thoughtSignature":"late"}]}}]}).to_string()).unwrap().is_none());
    let c = stream
        .push(&json!({"candidates":[{"finishReason":"STOP","content":{"parts":[]}}]}).to_string())
        .unwrap()
        .unwrap();
    let c: Value = serde_json::from_str(&c).unwrap();
    let calls = c["choices"][0]["delta"]["tool_calls"].as_array().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["function"]["arguments"], "{\"path\":\"a\"}");
    assert_eq!(calls[0]["thought_signature"]["data"], "late");
    assert_eq!(calls[0]["wire_ids"][0]["id"], "g1");
}

#[test]
fn indexless_atomic_chat_calls_keep_distinct_ids_and_signature_encoding() {
    let p = OpenAiCompatible::new("google-compatible", "https://example.test", None);
    let mut stream = p.stream_normalizer("gemini-3.8-flash");
    for (id, arg) in [("a", 1), ("b", 2)] {
        let event = json!({"choices":[{"delta":{"tool_calls":[{"id":id,"function":{"name":"read","arguments":format!("{{\"x\":{arg}}}")},"extra_content":{"google":{"thought_signature":"sig"}}}]}}]});
        assert!(stream.push(&event.to_string()).unwrap().is_none());
    }
    let out = stream
        .push(&json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}).to_string())
        .unwrap()
        .unwrap();
    let out: Value = serde_json::from_str(&out).unwrap();
    let calls = &out["choices"][0]["delta"]["tool_calls"];
    assert_eq!(calls.as_array().unwrap().len(), 2);
    assert_ne!(calls[0]["id"], calls[1]["id"]);
    assert_eq!(calls[0]["wire_ids"][0]["id"], "a");
    assert_eq!(calls[1]["wire_ids"][0]["id"], "b");
    let req = p
        .transform_request(
            "gemini-3.8-flash",
            &followup(json!({"role":"assistant","tool_calls":calls})),
        )
        .unwrap();
    assert_eq!(
        req.body["messages"][1]["tool_calls"][0]["extra_content"]["google"]["thought_signature"],
        "sig"
    );
}

#[test]
fn parallel_choices_finish_independently_and_keep_every_terminal_marker() {
    let p = OpenAiCompatible::new("custom", "", None);
    let mut stream = p.stream_normalizer("model");
    let mut chunks = Vec::new();
    for event in [
        json!({"model":"model","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"read","arguments":"{}"}}]}},{"index":1,"delta":{"tool_calls":[{"index":0,"id":"b","function":{"name":"read","arguments":"{"}}]}}]}),
        json!({"model":"model","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
        json!({"model":"model","choices":[{"index":1,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"x\":1}"}}]},"finish_reason":"tool_calls"}]}),
        json!({"choices":[],"usage":{"prompt_tokens_details":{"cached_tokens":3}}}),
    ] {
        if let Some(c) = stream.push(&event.to_string()).unwrap() {
            chunks.push(serde_json::from_str::<Value>(&c).unwrap());
        }
    }
    let final_chunk: Value = serde_json::from_str(&stream.finish().unwrap().unwrap()).unwrap();
    assert_eq!(final_chunk["choices"].as_array().unwrap().len(), 2);
    assert_eq!(final_chunk["model"], "model");
    assert_eq!(final_chunk["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(final_chunk["choices"][1]["finish_reason"], "tool_calls");
    assert_eq!(final_chunk["usage"]["cache_read_tokens"], 3);
    let tools = calls(chunks);
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0]["wire_ids"][0]["id"], "a");
    assert_eq!(tools[1]["wire_ids"][0]["id"], "b");
}

#[tokio::test]
async fn fallback_replays_each_target_from_the_unchanged_canonical_history() {
    use llmshim::{
        error::{Result, ShimError},
        provider::ProviderRequest,
        reasoning::ReplayTarget,
    };
    use std::sync::{Arc, Mutex};
    struct FailAfterPrepare {
        inner: OpenAi,
        seen: Arc<Mutex<Option<Value>>>,
    }
    impl Provider for FailAfterPrepare {
        fn name(&self) -> &str {
            "openai"
        }
        fn replay_target(&self, m: &str) -> ReplayTarget {
            self.inner.replay_target(m)
        }
        fn transform_request(&self, m: &str, r: &Value) -> Result<ProviderRequest> {
            *self.seen.lock().unwrap() = Some(self.inner.transform_request(m, r)?.body);
            Err(ShimError::ProviderError {
                status: 503,
                body: "synthetic failure".into(),
                retry_after: None,
            })
        }
        fn transform_response(&self, m: &str, r: Value) -> Result<Value> {
            self.inner.transform_response(m, r)
        }
        fn transform_stream_chunk(&self, m: &str, c: &str) -> Result<Option<String>> {
            self.inner.transform_stream_chunk(m, c)
        }
    }
    let source = OpenAi::new("test".into());
    let mut native = responses();
    native["output"].as_array_mut().unwrap().insert(
        0,
        json!({"type":"reasoning","id":"rs","summary":[],"encrypted_content":"opaque-reasoning"}),
    );
    let normalized = source.transform_response("gpt-6-astra", native).unwrap();
    let message = normalized["choices"][0]["message"].clone();
    let id = message["tool_calls"][0]["id"].clone();
    let request = followup(message);
    let original = request.clone();
    let seen = Arc::new(Mutex::new(None));
    let mut server = mockito::Server::new_async().await;
    let expected = json!({"model":"claude-sonnet-4-6","messages":[{"role":"user","content":"read"},{"role":"assistant","content":[{"type":"tool_use","id":id,"name":"read","input":{"path":"a"}}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":id,"content":"done"}]}]});
    let mock=server.mock("POST","/messages").match_body(mockito::Matcher::PartialJson(expected))
        .with_body(json!({"id":"a-success","stop_reason":"end_turn","content":[{"type":"text","text":"ok"}]}).to_string()).create_async().await;
    let router = llmshim::router::Router::new()
        .register(
            "openai",
            Box::new(FailAfterPrepare {
                inner: source,
                seen: seen.clone(),
            }),
        )
        .register(
            "anthropic",
            Box::new(Anthropic::new("test".into()).with_base_url(server.url())),
        );
    let config = llmshim::FallbackConfig::new(vec![
        "openai/gpt-6-astra".into(),
        "anthropic/claude-sonnet-4-6".into(),
    ])
    .max_retries(0);
    let result = llmshim::completion_with_fallback(&router, &request, &config, None)
        .await
        .unwrap();
    assert_eq!(result["choices"][0]["message"]["content"], "ok");
    assert_eq!(request, original);
    assert!(seen
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .to_string()
        .contains("opaque-reasoning"));
    mock.assert_async().await;
}

#[test]
fn repeated_native_response_ids_do_not_alias_owned_calls_across_turns() {
    let provider = Anthropic::new("test".into());
    let native = json!({"id":"reused-response","stop_reason":"tool_use","content":[{"type":"tool_use","id":"reused-call","name":"read","input":{}}]});
    let first = provider
        .transform_response("claude-sonnet-4-6", native.clone())
        .unwrap()["choices"][0]["message"]
        .clone();
    let second = provider
        .transform_response("claude-sonnet-4-6", native)
        .unwrap()["choices"][0]["message"]
        .clone();
    let a = &first["tool_calls"][0];
    let b = &second["tool_calls"][0];
    assert_ne!(a["id"], b["id"]);
    assert_ne!(a["wire_ids"][0]["scope"], b["wire_ids"][0]["scope"]);
    let request = json!({"messages":[{"role":"user","content":"read"},first,{"role":"tool","tool_call_id":a["id"],"content":"one"},{"role":"user","content":"read again"},second,{"role":"tool","tool_call_id":b["id"],"content":"two"}]});
    provider
        .transform_request("claude-sonnet-4-6", &request)
        .unwrap();
}
