use super::*;
use futures::StreamExt;

#[tokio::test]
async fn streamed_thinking_and_signatures_keep_vertex_origins_and_replay_only_for_same_binding() {
    for anthropic in [false, true] {
        let make = |binding| {
            let identity =
                GoogleIdentity::token_source("fixture-project", "global", binding, Arc::new(Fixed));
            if anthropic {
                Vertex::anthropic(identity)
            } else {
                Vertex::gemini(identity)
            }
        };
        let provider = make("fixture");
        let (model, wire, signature) = if anthropic {
            (
                "claude-sonnet-5-5@20261001",
                include_bytes!("../fixtures/vertex/anthropic-thinking.sse").to_vec(),
                "fixture-signature",
            )
        } else {
            (
                "gemini-3.8-flash",
                format!(
                    "data: {}\n\n",
                    include_str!("../fixtures/vertex/gemini.json").trim()
                )
                .into_bytes(),
                "Zml4dHVyZQ==",
            )
        };
        let source = futures::stream::iter(
            wire.into_iter()
                .map(|byte| Ok(bytes::Bytes::from(vec![byte]))),
        );
        let mut events = provider
            .transport()
            .unwrap()
            .events(model, Box::pin(source));
        let mut normalizer = provider.stream_normalizer(model);
        let mut reasoning = llmshim::reasoning::ReasoningAccumulator::default();
        let mut calls = Vec::new();
        while let Some(event) = events.next().await {
            if let Some(chunk) = normalizer.push(&event.unwrap()).unwrap() {
                let chunk: Value = serde_json::from_str(&chunk).unwrap();
                let delta = &chunk["choices"][0]["delta"];
                reasoning.push(delta).unwrap();
                calls.extend(
                    delta["tool_calls"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .cloned(),
                );
            }
        }
        normalizer.finish().unwrap();
        let blocks = reasoning.blocks();
        assert!(!blocks.is_empty());
        let expected = serde_json::to_value(provider.replay_target(model).origin()).unwrap();
        for block in &blocks {
            assert_origin(&block["origin"], &expected);
        }
        if anthropic {
            assert_eq!(blocks[0]["signature"], signature);
            assert_eq!(blocks[0]["text"], "Consider the weather.");
        } else {
            assert_eq!(calls[0]["thought_signature"]["data"], signature);
            assert_origin(&calls[0]["thought_signature"]["origin"], &expected);
        }
        assert_eq!(calls.len(), 1);
        let message = json!({"role":"assistant","content":null,"reasoning":blocks,
            "tool_calls":calls});
        let request = json!({"messages":[{"role":"user","content":"Question"},message,
            {"role":"tool","tool_call_id":calls[0]["id"],"content":"sunny"},
            {"role":"user","content":"Continue"}],"tools":[{"type":"function","function":{
            "name":"weather","parameters":{"type":"object","properties":{}}}}]});
        assert!(provider
            .transform_request(model, &request)
            .unwrap()
            .body
            .to_string()
            .contains(signature));
        assert!(!make("other")
            .transform_request(model, &request)
            .unwrap()
            .body
            .to_string()
            .contains(signature));
    }
}

fn assert_origin(actual: &Value, expected: &Value) {
    for key in ["provider", "model", "wire", "account", "family"] {
        assert_eq!(actual[key], expected[key], "origin {key}");
    }
}
