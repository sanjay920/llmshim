//! The self-hosted provider on the Responses wire, against a turn recorded
//! from an SGLang server: reasoning comes back as an item with an id, is
//! captured with this provider's origin, and goes back as the item the server
//! issued.
//!
//! Replay is gated on a known model family and the public catalog has never
//! heard of a served model, so every test first pins the catalog to a project
//! override that declares the fixture model's family. The catalog is a
//! process-wide lazy static — this file is its own binary for that reason.
use llmshim::provider::Provider;
use llmshim::providers::openai_compat::OpenAiCompatible;
use llmshim::reasoning::{ReasoningAccumulator, WireFormat};
use llmshim::streaming::StreamNormalizer;
use serde_json::{json, Value};

const MODEL: &str = "qwen3.8-27b-nvfp4";

fn pin_catalog() {
    std::env::set_var("LLMSHIM_CATALOG_OFFLINE", "1");
    std::env::set_var(
        "LLMSHIM_CATALOG_PROJECT",
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/sglang-models.toml"
        ),
    );
}

fn sglang(key: Option<&str>) -> OpenAiCompatible {
    pin_catalog();
    OpenAiCompatible::new(
        "sglang",
        "http://localhost:30000/v1/",
        key.map(str::to_string),
    )
    .with_wire(WireFormat::OpenAiResponses)
}

fn recorded_turn() -> Value {
    serde_json::from_str(include_str!("fixtures/sglang-responses-turn1.json")).unwrap()
}

fn tools() -> Value {
    json!([{"type": "function", "function": {"name": "get_weather",
        "description": "Current weather for a city",
        "parameters": {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]}}}])
}

fn turn1_request() -> Value {
    json!({"model": MODEL, "tools": tools(),
        "messages": [{"role": "user", "content": "What is the weather in Paris right now? Use the tool."}]})
}

/// The recorded turn, as the provider hands it back.
fn answered() -> Value {
    sglang(None)
        .transform_response(MODEL, recorded_turn())
        .expect("the recorded turn translates")
}

#[test]
fn the_responses_wire_posts_to_responses_stateless_and_keyless() {
    let sent = sglang(None)
        .transform_request(MODEL, &turn1_request())
        .unwrap();
    assert_eq!(sent.url, "http://localhost:30000/v1/responses");
    assert!(
        sent.headers.iter().all(|(k, _)| k != "Authorization"),
        "a keyless server gets no bearer"
    );
    assert_eq!(sent.body["model"], MODEL);
    assert_eq!(sent.body["store"], false);
    assert!(sent.body["include"]
        .as_array()
        .unwrap()
        .contains(&json!("reasoning.encrypted_content")));
    assert_eq!(sent.body["input"][0]["role"], "user");
    assert_eq!(
        sent.body["tools"][0]["name"], "get_weather",
        "flat Responses tool shape"
    );
}

#[test]
fn a_key_becomes_a_bearer_and_the_extension_namespace_is_this_servers() {
    let mut request = turn1_request();
    request["x-sglang"] = json!({"top_k": 20});
    let sent = sglang(Some("secret"))
        .transform_request(MODEL, &request)
        .unwrap();
    assert_eq!(
        sent.headers
            .iter()
            .find(|(k, _)| k == "Authorization")
            .unwrap()
            .1,
        "Bearer secret"
    );
    assert_eq!(sent.body["top_k"], 20);
    assert!(sent.body.get("x-sglang").is_none());
}

#[test]
fn the_default_wire_is_still_chat_completions() {
    pin_catalog();
    let sent = OpenAiCompatible::new("sglang", "http://localhost:30000/v1", None)
        .transform_request(MODEL, &turn1_request())
        .unwrap();
    assert_eq!(sent.url, "http://localhost:30000/v1/chat/completions");
    assert!(sent.body.get("input").is_none());
}

#[test]
fn a_recorded_turn_captures_reasoning_keyed_by_the_servers_item_id() {
    let answer = answered();
    let message = &answer["choices"][0]["message"];
    assert_eq!(answer["choices"][0]["finish_reason"], "tool_calls");
    assert!(
        message["tool_calls"][0]["id"]
            .as_str()
            .unwrap()
            .starts_with("call_ls_"),
        "llmshim mints the caller-facing id and maps it back to the server's on the wire"
    );
    assert_eq!(message["tool_calls"][0]["function"]["name"], "get_weather");
    let blocks = message["reasoning"]
        .as_array()
        .expect("reasoning was captured");
    assert_eq!(blocks.len(), 1);
    let block = &blocks[0];
    assert_eq!(block["item_id"], "rs_8f726712665348c2a0c5fd1d91aaa585");
    assert!(block["text"]
        .as_str()
        .unwrap()
        .starts_with("The user is asking"));
    assert_eq!(
        block["origin"]["provider"], "sglang",
        "the issuer, not the adapter borrowed"
    );
    assert_eq!(block["origin"]["wire"], "openai-responses");
    assert_eq!(
        block["origin"]["family"], "qwen",
        "declared by the project catalog"
    );
    assert_eq!(
        block["payload"]["encrypted_content"],
        Value::Null,
        "this server issues no encrypted content; the id is identity, not provenance"
    );
}

#[test]
fn the_captured_block_replays_as_the_item_the_server_issued() {
    let answer = answered();
    let assistant = answer["choices"][0]["message"].clone();
    let call_id = assistant["tool_calls"][0]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let mut turn2 = turn1_request();
    turn2["messages"].as_array_mut().unwrap().extend([
        assistant,
        json!({"role": "tool", "tool_call_id": call_id,
            "content": "{\"city\":\"Paris\",\"temp_c\":14,\"sky\":\"overcast\"}"}),
    ]);

    let sent = sglang(None).transform_request(MODEL, &turn2).unwrap();
    let input = sent.body["input"].as_array().unwrap();
    let reasoning = input
        .iter()
        .find(|item| item["type"] == "reasoning")
        .expect("the reasoning item went back");
    assert_eq!(reasoning["id"], "rs_8f726712665348c2a0c5fd1d91aaa585");
    assert_eq!(reasoning["content"][0]["type"], "reasoning_text");
    let call = input
        .iter()
        .find(|item| item["type"] == "function_call")
        .expect("the call went back");
    assert_eq!(call["call_id"], "call_bc844674ab13484ba1058fde");
    assert_eq!(
        call["id"], "fc_8ea88960",
        "the server's own item id rides along"
    );
    assert!(input
        .iter()
        .any(|item| item["type"] == "function_call_output"));
    let position = |kind: &str| input.iter().position(|item| item["type"] == kind).unwrap();
    assert!(
        position("reasoning") < position("function_call"),
        "thinking precedes the call"
    );

    // The same message on the Chat wire is a different wire from the one that
    // issued the block, so the block is dropped there rather than sent bare.
    pin_catalog();
    let chat = OpenAiCompatible::new("sglang", "http://localhost:30000/v1", None)
        .transform_request(MODEL, &turn2)
        .unwrap();
    let assistant = &chat.body["messages"][1];
    assert_eq!(assistant["role"], "assistant");
    assert!(assistant.get("reasoning_content").is_none());
    assert!(assistant.get("reasoning").is_none());
}

#[test]
fn a_served_model_with_no_declared_family_is_not_replayed() {
    let mut answer = answered();
    // Re-attribute the captured block to a model the catalog has no family
    // for; the tool call is dropped so only the reasoning is under test.
    let message = &mut answer["choices"][0]["message"];
    message["reasoning"][0]["origin"]["family"] = Value::Null;
    message["reasoning"][0]["origin"]["model"] = json!("other-served-model");
    message["content"] = json!("Let me check.");
    message.as_object_mut().unwrap().remove("tool_calls");
    let mut turn2 = turn1_request();
    turn2["model"] = json!("other-served-model");
    turn2["messages"]
        .as_array_mut()
        .unwrap()
        .push(answer["choices"][0]["message"].clone());
    let sent = sglang(None)
        .transform_request("other-served-model", &turn2)
        .unwrap();
    assert!(
        !sent.body["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["type"] == "reasoning"),
        "unknown family must not authorize replay — declare it in the catalog"
    );
}

#[test]
fn a_recorded_stream_folds_to_one_block_keyed_by_the_item_id_with_this_origin() {
    let provider = sglang(None);
    let mut normalizer: StreamNormalizer = provider.stream_normalizer(MODEL);
    let mut acc = ReasoningAccumulator::default();
    let mut content = String::new();
    let mut calls = Vec::new();
    for data in include_str!("fixtures/sglang-responses-stream.sse")
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
    {
        let Some(chunk) = normalizer.push(data).unwrap() else {
            continue;
        };
        let chunk: Value = serde_json::from_str(&chunk).unwrap();
        let delta = &chunk["choices"][0]["delta"];
        acc.push(delta).unwrap();
        content.push_str(delta["content"].as_str().unwrap_or(""));
        calls.extend(
            delta["tool_calls"]
                .as_array()
                .into_iter()
                .flatten()
                .cloned(),
        );
    }
    if let Some(chunk) = normalizer.finish().unwrap() {
        let chunk: Value = serde_json::from_str(&chunk).unwrap();
        acc.push(&chunk["choices"][0]["delta"]).unwrap();
        calls.extend(
            chunk["choices"][0]["delta"]["tool_calls"]
                .as_array()
                .into_iter()
                .flatten()
                .cloned(),
        );
    }
    let blocks = acc.blocks();
    assert_eq!(
        blocks.len(),
        1,
        "seven deltas and a completed item are one block"
    );
    assert_eq!(blocks[0]["item_id"], "rs_31c1a83ee7ea4f9997f68a939c262410");
    assert!(blocks[0]["text"]
        .as_str()
        .unwrap()
        .starts_with("The user wants the current weather"));
    assert_eq!(
        blocks[0]["origin"]["provider"], "sglang",
        "rebound to the issuing target"
    );
    assert_eq!(blocks[0]["origin"]["wire"], "openai-responses");
    assert_eq!(content, "\n\n");
    assert_eq!(calls.len(), 1, "one complete call, emitted once");
    assert_eq!(calls[0]["function"]["name"], "get_weather");
}
