use llmshim::catalog::ModelFamily;
use llmshim::{
    provider::Provider,
    providers::{
        anthropic::Anthropic, gemini::Gemini, openai::OpenAi, openai_compat::OpenAiCompatible,
        openrouter::OpenRouter,
    },
    reasoning::*,
};
use serde_json::{json, Value};

fn request(message: Value) -> Value {
    json!({"messages":[{"role":"user","content":"question"},message,{"role":"user","content":"continue"}]})
}
fn anthropic_native() -> Value {
    json!({"id":"a1","stop_reason":"end_turn","content":[
    {"type":"thinking","thinking":"first\nthought","signature":"first-sig+/="},
    {"type":"redacted_thinking","data":"opaque+/=="},
    {"type":"thinking","thinking":"second","signature":"second-sig"},
    {"type":"text","text":"answer"}
],"usage":{}})
}

#[test]
fn multiple_anthropic_blocks_keep_order_text_and_opaque_bytes() {
    let p = Anthropic::new("key".into());
    let raw = anthropic_native();
    let r = p
        .transform_response("claude-sonnet-4-6", raw.clone())
        .unwrap();
    let message = &r["choices"][0]["message"];
    assert_eq!(message["reasoning"].as_array().unwrap().len(), 3);
    assert!(message.get("reasoning_content").is_none());
    assert!(message.get("reasoning_signature").is_none());
    assert!(message.get("redacted_reasoning_content").is_none());
    let first: ReasoningBlock = serde_json::from_value(message["reasoning"][0].clone()).unwrap();
    assert_eq!(first.origin.family, Some(ModelFamily::Claude));
    assert_eq!(first.origin.model, "claude-sonnet-4-6");
    let replay = p
        .transform_request("claude-opus-5", &request(message.clone()))
        .unwrap();
    assert_eq!(replay.body["messages"][1]["content"], raw["content"]);
}

#[test]
fn missing_origin_legacy_and_raw_native_thinking_are_dropped() {
    let p = Anthropic::new("key".into());
    let message = json!({"role":"assistant","content":[{"type":"thinking","thinking":"raw","signature":"untracked"},{"type":"text","text":"ok"}],
        "reasoning_content":"legacy","reasoning_signature":"legacy-sig","redacted_reasoning_content":"secret"});
    let r = p
        .transform_request("claude-sonnet-4-6", &request(message))
        .unwrap();
    assert_eq!(
        r.body["messages"][1]["content"],
        json!([{"type":"text","text":"ok"}])
    );
}

#[test]
fn replay_policy_is_family_wire_and_account_gated() {
    let a = ReplayTarget::new(
        "anthropic",
        "claude-sonnet-4-6",
        WireFormat::AnthropicMessages,
    )
    .bind_account("https://api.example", Some("one"));
    let origin = a.origin();
    let mut target = a.clone();
    target.provider = "another-claude-gateway".into();
    target.account = None;
    assert_eq!(should_replay(&origin, ReasoningKind::Text, &target), Ok(()));
    target.wire = WireFormat::OpenAiChat;
    assert_eq!(
        should_replay(&origin, ReasoningKind::Text, &target),
        Err(DropReason::WireMismatch)
    );
    target.wire = a.wire;
    target.family = Some(ModelFamily::Qwen);
    assert_eq!(
        should_replay(&origin, ReasoningKind::Text, &target),
        Err(DropReason::FamilyMismatch)
    );
    target.family = None;
    assert_eq!(
        should_replay(&origin, ReasoningKind::Text, &target),
        Err(DropReason::UnknownFamily)
    );
    assert_eq!(should_replay(&origin, ReasoningKind::Encrypted, &a), Ok(()));
    target = a.clone().bind_account("https://api.example", Some("two"));
    assert_eq!(
        should_replay(&origin, ReasoningKind::Encrypted, &target),
        Err(DropReason::AccountMismatch)
    );
    target = a.clone();
    target.account = None;
    assert_eq!(
        should_replay(&origin, ReasoningKind::Encrypted, &target),
        Err(DropReason::AccountMismatch)
    );
}

#[test]
fn responses_preserve_encrypted_items_and_refuse_other_accounts() {
    let p = OpenAi::new("account-a".into());
    let item = json!({"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"summary"}],"content":[{"type":"reasoning_text","text":"native"}],"encrypted_content":"encrypted+/=="});
    let raw = json!({"id":"r1","status":"completed","output":[item.clone()],"usage":{}});
    let normalized = p.transform_response("gpt-6-astra", raw).unwrap();
    let message = normalized["choices"][0]["message"].clone();
    assert_eq!(message["reasoning"][0]["kind"], "encrypted");
    assert_eq!(message["reasoning"][0]["data"], "encrypted+/==");
    assert!(!message.to_string().contains("account-a"));
    let r = p
        .transform_request("gpt-5.6-sol", &request(message.clone()))
        .unwrap();
    assert_eq!(r.body["input"][1], item);
    assert_eq!(r.body["store"], false);
    assert!(r.body["include"]
        .as_array()
        .unwrap()
        .contains(&json!("reasoning.encrypted_content")));
    let other = OpenAi::new("account-b".into())
        .transform_request("gpt-6-astra", &request(message))
        .unwrap();
    assert!(!other.body.to_string().contains("encrypted+/=="));
}

#[test]
fn native_overrides_cannot_enable_provider_storage() {
    for conversation in [json!("conv_other"), json!({"id": "conv_other"})] {
        let request = json!({
            "messages": [{"role": "user", "content": "explicit history"}],
            "store": true,
            "x-openai": {
                "store": true,
                "previous_response_id": "stored",
                "conversation": conversation,
                "include": ["message.output_text.logprobs"]
            }
        });
        let stored_request = request.clone();
        let outgoing = OpenAi::new("key".into())
            .transform_request("gpt-6-astra", &request)
            .unwrap();
        assert_eq!(outgoing.body["store"], false);
        assert!(outgoing.body.get("previous_response_id").is_none());
        assert!(outgoing.body.get("conversation").is_none());
        assert_eq!(
            outgoing.body["include"],
            json!([
                "message.output_text.logprobs",
                "reasoning.encrypted_content"
            ])
        );
        assert_eq!(request, stored_request);
    }
}

#[test]
fn native_overrides_and_mislabelled_payloads_cannot_bypass_replay_policy() {
    let p = OpenAi::new("key".into());
    let input = json!({"messages":[],"x-openai":{"input":[{"type":"reasoning","id":"rs_untracked","encrypted_content":"untracked"},{"role":"user","content":"hello"}]}});
    let request = p.transform_request("gpt-6-astra", &input).unwrap();
    assert_eq!(
        request.body["input"],
        json!([{"role":"user","content":"hello"}])
    );
    let origin = p.replay_target("gpt-6-astra").origin();
    let mut message = json!({"role":"assistant","reasoning":[{"kind":"text","text":"summary","origin":origin,
        "payload":{"type":"reasoning","id":"rs_bad","encrypted_content":"hidden"}}]});
    filter_reasoning_for_target(&mut message, &p.replay_target("gpt-6-astra"));
    assert!(message.get("reasoning").is_none());
}

#[test]
fn structured_chat_reasoning_fragments_assemble_the_original_payload() {
    let p = OpenRouter::new("key".into());
    let mut acc = ReasoningAccumulator::default();
    for text in ["first ", "second"] {
        let raw = json!({"choices":[{"delta":{"reasoning_details":[{"type":"reasoning.text","text":text,"index":2}]}}]});
        let chunk = p
            .transform_stream_chunk("deepseek/deepseek-r1", &raw.to_string())
            .unwrap()
            .unwrap();
        acc.push(&serde_json::from_str::<Value>(&chunk).unwrap()["choices"][0]["delta"])
            .unwrap();
    }
    let r = p
        .transform_request(
            "deepseek/deepseek-r1",
            &request(json!({"role":"assistant","reasoning":acc.blocks()})),
        )
        .unwrap();
    assert_eq!(
        r.body["messages"][1]["reasoning_details"][0]["text"],
        "first second"
    );
}

#[test]
fn deepseek_reasoning_echoes_to_same_family_and_drops_on_cross_family_hops() {
    let p = OpenAiCompatible::new("deepseek", "https://api.example", Some("key".into()));
    let model = "deepseek-v4-pro";
    let r=p.transform_response(model,json!({"choices":[{"message":{"role":"assistant","content":"answer","reasoning_content":"exact\nreasoning"}}]})).unwrap();
    let message = r["choices"][0]["message"].clone();
    assert_eq!(message["reasoning"][0]["origin"]["family"], "deepseek");
    let replay = p
        .transform_request(model, &request(message.clone()))
        .unwrap();
    assert_eq!(
        replay.body["messages"][1]["reasoning_content"],
        "exact\nreasoning"
    );
    let catalog = llmshim::catalog::global().unwrap().snapshot();
    let qwen = catalog
        .models()
        .find(|m| m.provider == "openrouter" && m.family == Some(ModelFamily::Qwen))
        .unwrap();
    let other_provider = OpenRouter::new("key".into());
    assert_eq!(
        other_provider.replay_target(&qwen.name).family,
        Some(ModelFamily::Qwen)
    );
    let other = other_provider
        .transform_request(&qwen.name, &request(message))
        .unwrap();
    assert!(other.body["messages"][1].get("reasoning_content").is_none());
    assert!(other.body["messages"][1].get("reasoning").is_none());
}

#[test]
fn family_lookup_normalizes_an_openrouter_variant_suffix_but_keeps_the_wire_id() {
    // MOH-240: a suffixed OpenRouter slug (`:nitro`, `:floor`, …) missed the
    // catalog entirely, so its family came back `None` and reasoning replay
    // was dropped as `unknown_family`. The lookup should normalize; the id
    // ReplayTarget stores (and that goes out on the wire) must not change.
    let base = ReplayTarget::new(
        "openrouter",
        "deepseek/deepseek-v4.1-flash",
        WireFormat::OpenAiChat,
    );
    assert_eq!(base.family, Some(ModelFamily::Deepseek));

    for suffix in [":nitro", ":floor", ":free", ":exacto", ":online"] {
        let model = format!("deepseek/deepseek-v4.1-flash{suffix}");
        let suffixed = ReplayTarget::new("openrouter", &model, WireFormat::OpenAiChat);
        assert_eq!(
            suffixed.family,
            Some(ModelFamily::Deepseek),
            "suffix {suffix} must still resolve a family"
        );
        assert_eq!(
            suffixed.model, model,
            "the suffix must survive on the target"
        );
    }

    // An unrecognized suffix must not be treated as an OpenRouter variant.
    let unknown = ReplayTarget::new(
        "openrouter",
        "deepseek/deepseek-v4.1-flash:beta",
        WireFormat::OpenAiChat,
    );
    assert_eq!(unknown.family, None);
}

#[test]
fn chat_details_preserve_objects_and_encoding_with_provenance() {
    let p = OpenRouter::new("key".into());
    let details = json!([{"type":"reasoning.text","text":"first","signature":"sig","format":"anthropic-claude-v1","index":0},{"type":"reasoning.encrypted","data":"opaque","id":"rs_a","index":1}]);
    let r=p.transform_response("anthropic/claude-sonnet-4.6",json!({"choices":[{"message":{"role":"assistant","content":"answer","reasoning_details":details}}]})).unwrap();
    let message = r["choices"][0]["message"].clone();
    assert!(message.get("reasoning_details").is_none());
    let replay = p
        .transform_request("anthropic/claude-sonnet-4.6", &request(message.clone()))
        .unwrap();
    assert_eq!(replay.body["messages"][1]["reasoning_details"], details);
    let native = Anthropic::new("key".into())
        .transform_request("claude-sonnet-4-6", &request(message))
        .unwrap();
    assert!(!native.body.to_string().contains("sig"));
}

#[test]
fn gemini_tool_signatures_have_origin_and_round_trip_only_to_compatible_wire() {
    let p = Gemini::new("key".into());
    let raw = json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":"read","args":{}},"thoughtSignature":"opaque-sig"}]},"finishReason":"STOP"}]});
    let r = p.transform_response("gemini-3.8-flash", raw).unwrap();
    let message = r["choices"][0]["message"].clone();
    assert_eq!(
        message["tool_calls"][0]["thought_signature"]["data"],
        "opaque-sig"
    );
    assert_eq!(
        message["tool_calls"][0]["thought_signature"]["origin"]["wire"],
        "google-generate-content"
    );
    let mut foreign = message.clone();
    filter_reasoning_for_target(
        &mut foreign,
        &OpenAi::new("key".into()).replay_target("gpt-6-astra"),
    );
    assert!(foreign["tool_calls"][0].get("thought_signature").is_none());
    let mut other_provider = message.clone();
    let mut same_family_target = p.replay_target("gemini-3.8-flash");
    same_family_target.provider = "another-google-gateway".into();
    filter_reasoning_for_target(&mut other_provider, &same_family_target);
    assert!(other_provider["tool_calls"][0]
        .get("thought_signature")
        .is_none());
    let input = json!({"messages":[{"role":"user","content":"read"},message,{"role":"tool","tool_call_id":r["choices"][0]["message"]["tool_calls"][0]["id"],"name":"read","content":"result"}]});
    let req = p.transform_request("gemini-3.8-flash", &input).unwrap();
    assert_eq!(
        req.body["contents"][1]["parts"][0]["thoughtSignature"],
        "opaque-sig"
    );
}

#[test]
fn streamed_anthropic_blocks_assemble_separately_and_replay_losslessly() {
    let p = Anthropic::new("key".into());
    let mut acc = ReasoningAccumulator::default();
    let events = [
        json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"part "}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"one"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig"}}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"redacted_thinking","data":"redacted"}}),
    ];
    for event in events {
        let chunk = p
            .transform_stream_chunk("claude-sonnet-4-6", &event.to_string())
            .unwrap()
            .unwrap();
        let v: Value = serde_json::from_str(&chunk).unwrap();
        assert!(v["choices"][0]["delta"].get("reasoning_content").is_none());
        acc.push(&v["choices"][0]["delta"]).unwrap();
    }
    let req = p
        .transform_request(
            "claude-sonnet-4-6",
            &request(json!({"role":"assistant","content":"answer","reasoning":acc.blocks()})),
        )
        .unwrap();
    assert_eq!(
        req.body["messages"][1]["content"],
        json!([
            {"type":"thinking","thinking":"part one","signature":"sig"},
            {"type":"redacted_thinking","data":"redacted"},{"type":"text","text":"answer"}
        ])
    );
}

#[test]
fn completed_responses_item_replaces_summary_fragments_without_duplication() {
    let p = OpenAi::new("key".into());
    let mut acc = ReasoningAccumulator::default();
    let item = json!({"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"summary"}],"encrypted_content":"opaque"});
    for event in [
        json!({"type":"response.reasoning_summary_text.delta","item_id":"rs_1","output_index":0,"delta":"summary"}),
        json!({"type":"response.output_item.done","output_index":0,"item":item.clone()}),
        json!({"type":"response.completed","response":{"status":"completed","output":[item.clone()]}}),
    ] {
        let chunk = p
            .transform_stream_chunk("gpt-6-astra", &event.to_string())
            .unwrap()
            .unwrap();
        acc.push(&serde_json::from_str::<Value>(&chunk).unwrap()["choices"][0]["delta"])
            .unwrap();
    }
    assert_eq!(acc.blocks().len(), 1);
    let req = p
        .transform_request(
            "gpt-6-astra",
            &request(json!({"role":"assistant","reasoning":acc.blocks()})),
        )
        .unwrap();
    assert_eq!(req.body["input"][1], item);
}

/// A buffered answer arrives with whole blocks that carry neither an item id
/// nor a stream index. Two such blocks — one readable, one encrypted — must
/// stay two: merged under one key, the encrypted one swallowed the text.
#[test]
fn unkeyed_blocks_in_one_message_stay_separate_blocks() {
    let origin = ReplayTarget::new(
        "anthropic",
        "claude-sonnet-5",
        WireFormat::AnthropicMessages,
    )
    .origin();
    let mut acc = ReasoningAccumulator::default();
    acc.push(&json!({"reasoning": [
        {"kind": "text", "text": "first thought", "origin": origin},
        {"kind": "encrypted", "data": "opaque", "origin": origin},
    ]}))
    .unwrap();
    let blocks = acc.blocks();
    assert_eq!(blocks.len(), 2, "two unkeyed blocks in, two blocks out");
    assert_eq!(blocks[0]["text"], "first thought");
    assert!(blocks[0].get("data").is_none());
    assert_eq!(blocks[1]["data"], "opaque");
    assert!(blocks[1].get("text").is_none());
}

/// Streamed bare fragments still merge: each carries the index the transport
/// stamped, so successive chunks of one block land under one key as before.
#[test]
fn indexed_fragments_across_chunks_still_assemble_one_block() {
    let mut acc = ReasoningAccumulator::default();
    acc.push(&json!({"reasoning": [{"index": 0, "text": "the file "}]}))
        .unwrap();
    acc.push(&json!({"reasoning": [{"index": 0, "text": "needs replacing"}]}))
        .unwrap();
    let blocks = acc.blocks();
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0]["text"], "the file needs replacing");
}

/// The buffered path end to end: `shim::chunks` frames an assembled answer as
/// one chunk whose blocks have no index, and folding that chunk through the
/// accumulator gives the blocks back unmerged.
#[test]
fn a_buffered_chunk_folds_back_into_its_separate_blocks() {
    let origin = ReplayTarget::new(
        "anthropic",
        "claude-sonnet-5",
        WireFormat::AnthropicMessages,
    )
    .origin();
    let response = json!({"object": "chat.completion", "choices": [{"index": 0, "message": {
        "role": "assistant", "content": "writing it now",
        "reasoning": [
            {"kind": "text", "text": "first thought", "origin": origin},
            {"kind": "encrypted", "data": "opaque", "origin": origin},
        ]}, "finish_reason": "stop"}]});
    let chunks = llmshim::shim::chunks(response);
    assert_eq!(chunks.len(), 1);
    let chunk: Value = serde_json::from_str(chunks[0].as_ref().unwrap()).unwrap();
    assert_eq!(chunk["object"], "chat.completion.chunk");
    let mut acc = ReasoningAccumulator::default();
    acc.push(&chunk["choices"][0]["delta"]).unwrap();
    assert_eq!(acc.blocks().len(), 2);
}
