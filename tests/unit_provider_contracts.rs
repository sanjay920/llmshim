//! Every shipped Provider implementation must be exercised by this contract.
use llmshim::{
    catalog::ModelFamily,
    provider::Provider,
    providers::{
        anthropic::Anthropic,
        chatgpt::{ChatGpt, ChatGptAuth},
        gemini::Gemini,
        openai::OpenAi,
        openai_compat::OpenAiCompatible,
        openrouter::OpenRouter,
        xai::Xai,
    },
    reasoning::{ReasoningBlock, ReplayTarget, WireFormat},
};
use serde_json::{json, Value};
use std::{collections::BTreeSet, path::Path};
use syn::visit::Visit;

#[derive(Default)]
struct Inventory(BTreeSet<String>);
impl<'ast> Visit<'ast> for Inventory {
    fn visit_item_impl(&mut self, item: &'ast syn::ItemImpl) {
        if item
            .trait_
            .as_ref()
            .is_some_and(|(_, path, _)| path.segments.last().is_some_and(|s| s.ident == "Provider"))
        {
            let syn::Type::Path(ty) = item.self_ty.as_ref() else {
                panic!("Provider fixture needs a concrete type");
            };
            self.0
                .insert(ty.path.segments.last().unwrap().ident.to_string());
        }
        syn::visit::visit_item_impl(self, item);
    }
}
fn enumerate(dir: &Path, inventory: &mut Inventory) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            enumerate(&path, inventory);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            inventory
                .visit_file(&syn::parse_file(&std::fs::read_to_string(path).unwrap()).unwrap());
        }
    }
}

fn providers(auth: &ChatGptAuth) -> Vec<(&'static str, &'static str, Box<dyn Provider>)> {
    vec![
        (
            "Anthropic",
            "claude-sonnet-4-6",
            Box::new(Anthropic::new("test".into())),
        ),
        (
            "ChatGpt",
            "gpt-6-astra",
            Box::new(ChatGpt::new(auth.clone())),
        ),
        (
            "Gemini",
            "gemini-2.5-flash",
            Box::new(Gemini::new("test".into())),
        ),
        (
            "OpenAi",
            "gpt-6-astra",
            Box::new(OpenAi::new("test".into())),
        ),
        (
            "OpenAiCompatible",
            "gpt-6-astra",
            Box::new(OpenAiCompatible::new("vllm", "http://localhost", None)),
        ),
        (
            "OpenRouter",
            "anthropic/claude-sonnet-4-6",
            Box::new(OpenRouter::new("test".into())),
        ),
        ("Xai", "grok-4.6", Box::new(Xai::new("test".into()))),
    ]
}
fn native(wire: WireFormat) -> Value {
    match wire {
        WireFormat::AnthropicMessages => {
            json!({"stop_reason":"end_turn","content":[{"type":"thinking","thinking":"ALLOW_REASONING","signature":"ALLOW_SIGNATURE"},{"type":"text","text":"answer"}]})
        }
        WireFormat::OpenAiResponses => {
            json!({"status":"completed","output":[{"type":"reasoning","id":"rs1","summary":[{"type":"summary_text","text":"ALLOW_REASONING"}],"encrypted_content":"ALLOW_ENCRYPTED"},{"type":"message","content":[{"type":"output_text","text":"answer"}]}]})
        }
        WireFormat::GoogleGenerateContent => {
            json!({"candidates":[{"finishReason":"STOP","content":{"role":"model","parts":[{"thought":true,"text":"ALLOW_REASONING","thoughtSignature":"ALLOW_SIGNATURE"},{"text":"answer"}]}}]})
        }
        WireFormat::OpenAiChat => {
            json!({"choices":[{"index":0,"finish_reason":"stop","message":{"role":"assistant","content":"answer","reasoning_content":"ALLOW_REASONING"}}]})
        }
    }
}

#[test]
fn every_provider_filters_reasoning_and_validates_tools_before_serialization() {
    let dir = tempfile::tempdir().unwrap();
    let auth = ChatGptAuth::new(dir.path().join("auth.json"));
    std::fs::write(auth.auth_path(),json!({"access_token":"test-access","refresh_token":"test-refresh","account_id":"test-account","expires_at":chrono::Utc::now().timestamp()+3600}).to_string()).unwrap();
    let fixtures = providers(&auth);
    let mut inventory = Inventory::default();
    enumerate(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut inventory,
    );
    let covered = fixtures
        .iter()
        .map(|(name, _, _)| name.to_string())
        .collect();
    assert_eq!(
        inventory.0, covered,
        "Add every new Provider implementation to the replay/validation fixtures"
    );

    for (name, model, provider) in fixtures {
        let target = provider.replay_target(model);
        let normalized = provider
            .transform_response(model, native(target.wire))
            .unwrap();
        let original_message = normalized["choices"][0]["message"].clone();
        let request = json!({"messages":[{"role":"user","content":"question"},original_message,{"role":"user","content":"continue"}]});
        let outbound = provider
            .transform_request(model, &request)
            .unwrap()
            .body
            .to_string();
        assert!(
            outbound.contains("ALLOW_REASONING"),
            "{name} dropped compatible reasoning"
        );

        let mut foreign = request.clone();
        for block in foreign["messages"][1]["reasoning"].as_array_mut().unwrap() {
            block["origin"]["family"] = json!("llama");
        }
        let outbound = provider
            .transform_request(model, &foreign)
            .unwrap()
            .body
            .to_string();
        assert!(
            !outbound.contains("ALLOW_"),
            "{name} forwarded foreign-family reasoning"
        );

        let mut untracked = request.clone();
        for block in untracked["messages"][1]["reasoning"]
            .as_array_mut()
            .unwrap()
        {
            block.as_object_mut().unwrap().remove("origin");
        }
        untracked["messages"][1]["reasoning_content"] = json!("BLOCK_UNTRACKED");
        let outbound = provider
            .transform_request(model, &untracked)
            .unwrap()
            .body
            .to_string();
        assert!(
            !outbound.contains("ALLOW_") && !outbound.contains("BLOCK_UNTRACKED"),
            "{name} forwarded untracked reasoning"
        );

        let mut foreign_target = ReplayTarget::new("foreign", "unknown", target.wire);
        foreign_target.family = Some(ModelFamily::Llama);
        let source=Anthropic::new("test".into()).transform_response("claude-sonnet-4-6",json!({"id":"r","stop_reason":"tool_use","content":[{"type":"tool_use","id":"native-call","name":"read","input":{}}]})).unwrap();
        let mut assistant = source["choices"][0]["message"].clone();
        assistant["tool_calls"][0]["thought_signature"] =
            json!({"data":"BLOCK_SIGNATURE","origin":foreign_target.origin()});
        assistant["reasoning"] = json!([ReasoningBlock::text(
            "BLOCK_REASONING",
            foreign_target.origin()
        )]);
        let id = assistant["tool_calls"][0]["id"].clone();
        let history = json!({"messages":[{"role":"user","content":"read"},assistant,{"role":"tool","tool_call_id":id,"content":"done"},{"role":"user","content":"continue"}]});
        let before = history.clone();
        let outbound = provider
            .transform_request(model, &history)
            .unwrap()
            .body
            .to_string();
        assert!(
            !outbound.contains("BLOCK_"),
            "{name} forwarded incompatible tool metadata"
        );
        assert_eq!(history, before);

        let mut pending = history.clone();
        pending["messages"].as_array_mut().unwrap().remove(2);
        assert!(
            matches!(
                provider.transform_request(model, &pending),
                Err(llmshim::error::ShimError::ProviderError { status: 400, .. })
            ),
            "{name} accepted unpaired tools"
        );
        for bad in [json!({}), json!("bad"), json!(true), json!(1)] {
            let malformed =
                json!({"messages":[{"role":"assistant","content":"answer","tool_calls":bad}]});
            assert!(
                matches!(
                    provider.transform_request(model, &malformed),
                    Err(llmshim::error::ShimError::ProviderError { status: 400, .. })
                ),
                "{name} accepted malformed tool_calls"
            );
        }
        for empty in [Value::Null, json!([])] {
            provider.transform_request(model,&json!({"messages":[{"role":"assistant","content":"answer","tool_calls":empty},{"role":"user","content":"continue"}]})).unwrap();
        }
    }
}

/// Two same-role messages side by side — what a harness produces when it
/// compacts a window and the summary lands beside a kept assistant turn. Only
/// a wire that rejects adjacency may fold them (Gemini does); everywhere else
/// the boundary is the caller's and must survive the transform. The inventory
/// check above means a new adapter has to declare which side it is on.
#[test]
fn same_role_adjacency_is_merged_only_where_the_wire_rejects_it() {
    let dir = tempfile::tempdir().unwrap();
    let auth = ChatGptAuth::new(dir.path().join("auth.json"));
    std::fs::write(auth.auth_path(),json!({"access_token":"test-access","refresh_token":"test-refresh","account_id":"test-account","expires_at":chrono::Utc::now().timestamp()+3600}).to_string()).unwrap();
    let request = json!({"messages":[
        {"role":"user","content":"question"},
        {"role":"assistant","content":"first"},
        {"role":"assistant","content":"second"},
        {"role":"user","content":"continue"}]});

    for (name, model, provider) in providers(&auth) {
        let wire = provider.replay_target(model).wire;
        let body = provider.transform_request(model, &request).unwrap().body;
        let (turns, assistant) = match wire {
            WireFormat::GoogleGenerateContent => (&body["contents"], "model"),
            WireFormat::OpenAiResponses => (&body["input"], "assistant"),
            WireFormat::AnthropicMessages | WireFormat::OpenAiChat => {
                (&body["messages"], "assistant")
            }
        };
        let assistant_turns: Vec<_> = turns
            .as_array()
            .unwrap()
            .iter()
            .filter(|turn| turn["role"] == assistant)
            .collect();
        let text = body.to_string();
        assert!(
            text.contains("first") && text.contains("second"),
            "{name} lost a message's content"
        );
        if wire == WireFormat::GoogleGenerateContent {
            assert_eq!(
                assistant_turns.len(),
                1,
                "{name} must fold adjacent turns; its wire rejects them"
            );
        } else {
            assert_eq!(
                assistant_turns.len(),
                2,
                "{name} must keep the caller's message boundaries; its wire accepts them"
            );
        }
    }
}
