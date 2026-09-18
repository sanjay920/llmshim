use futures::StreamExt;
use llmshim::provider::Provider;
use llmshim::providers::{
    anthropic::Anthropic,
    chatgpt::{ChatGpt, ChatGptAuth},
    gemini::Gemini,
    openai::OpenAi,
    openai_compat::OpenAiCompatible,
    openrouter::OpenRouter,
    xai::Xai,
};
use serde_json::{json, Value};

#[test]
fn cache_counters_cover_every_transport_and_never_count_negative_values() {
    for (native, reads, writes) in [
        (
            json!({"cache_read_input_tokens":31,"cache_creation_input_tokens":17}),
            31,
            17,
        ),
        (json!({"input_tokens_details":{"cached_tokens":32}}), 32, 0),
        (
            json!({"prompt_tokens_details":{"cached_tokens":33,"cache_write_tokens":18}}),
            33,
            18,
        ),
        (json!({"cachedContentTokenCount":34}), 34, 0),
        (json!({"prompt_cache_hit_tokens":35}), 35, 0),
        (
            json!({"cache_read_tokens":0,"cache_read_input_tokens":99,"cache_write_tokens":19}),
            0,
            19,
        ),
        (
            json!({"cache_read_tokens":-1,"cache_write_tokens":"not-a-count"}),
            0,
            0,
        ),
        (json!({}), 0, 0),
    ] {
        let mut normalized = json!({"prompt_tokens":42});
        llmshim::usage::normalize_cache(&native, &mut normalized);
        assert_eq!(normalized["cache_read_tokens"], reads);
        assert_eq!(normalized["cache_write_tokens"], writes);
        assert_eq!(normalized["prompt_tokens"], 42);
    }
}

fn responses_body() -> Value {
    json!({"id":"r1","status":"completed","output":[],"usage":{"input_tokens":50,"output_tokens":3,"total_tokens":53,"input_tokens_details":{"cached_tokens":40}}})
}

#[test]
fn every_provider_exposes_canonical_cache_usage_for_complete_responses() {
    let providers: Vec<(Box<dyn Provider>, Value, u64, u64)> = vec![
        (
            Box::new(OpenAi::new("test".into())),
            responses_body(),
            40,
            0,
        ),
        (Box::new(Xai::new("test".into())), responses_body(), 40, 0),
        (
            Box::new(ChatGpt::new(ChatGptAuth::new(
                std::env::temp_dir().join("llmshim-no-auth-fixture/auth.json"),
            ))),
            responses_body(),
            40,
            0,
        ),
        (
            Box::new(Anthropic::new("test".into())),
            json!({"id":"a1","stop_reason":"end_turn","content":[],"usage":{"input_tokens":5,"output_tokens":3,"cache_read_input_tokens":40,"cache_creation_input_tokens":9}}),
            40,
            9,
        ),
        (
            Box::new(Gemini::new("test".into())),
            json!({"candidates":[{"content":{"parts":[{"text":"ok"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":50,"cachedContentTokenCount":40}}),
            40,
            0,
        ),
        (
            Box::new(OpenRouter::new("test".into())),
            json!({"choices":[],"usage":{"prompt_tokens_details":{"cached_tokens":40,"cache_write_tokens":9}}}),
            40,
            9,
        ),
        (
            Box::new(OpenAiCompatible::new("vllm", "", None)),
            json!({"choices":[],"usage":{"prompt_tokens_details":{"cached_tokens":40}}}),
            40,
            0,
        ),
        (
            Box::new(OpenAiCompatible::new("sglang", "", None)),
            json!({"choices":[],"usage":{}}),
            0,
            0,
        ),
        (
            Box::new(OpenAiCompatible::new("deepseek", "", None)),
            json!({"choices":[],"usage":{"prompt_cache_hit_tokens":40}}),
            40,
            0,
        ),
    ];
    for (provider, native, reads, writes) in providers {
        let result = provider.transform_response("gpt-6-astra", native).unwrap();
        assert_eq!(
            result["usage"]["cache_read_tokens"],
            reads,
            "{}",
            provider.name()
        );
        assert_eq!(
            result["usage"]["cache_write_tokens"],
            writes,
            "{}",
            provider.name()
        );
        let log = llmshim::log::LogEntry::from_response(
            provider.name(),
            "test",
            &result,
            std::time::Duration::ZERO,
        );
        assert_eq!(log.cache_read_tokens, reads);
        assert_eq!(log.cache_write_tokens, writes);
    }
}

#[test]
fn every_stream_translator_exposes_cache_counts() {
    let terminal = json!({"type":"response.completed","response":responses_body()});
    let providers: Vec<(Box<dyn Provider>, Value)> = vec![
        (Box::new(OpenAi::new("test".into())), terminal.clone()),
        (Box::new(Xai::new("test".into())), terminal.clone()),
        (
            Box::new(ChatGpt::new(ChatGptAuth::new(
                std::env::temp_dir().join("llmshim-no-auth-fixture/auth.json"),
            ))),
            terminal,
        ),
        (
            Box::new(Anthropic::new("test".into())),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"cache_read_input_tokens":40}}),
        ),
        (
            Box::new(Gemini::new("test".into())),
            json!({"candidates":[{"content":{"parts":[{"text":"ok"}]},"finishReason":"STOP"}],"usageMetadata":{"cachedContentTokenCount":40}}),
        ),
        (
            Box::new(OpenRouter::new("test".into())),
            json!({"choices":[],"usage":{"prompt_tokens_details":{"cached_tokens":40}}}),
        ),
        (
            Box::new(OpenAiCompatible::new("vllm", "", None)),
            json!({"choices":[],"usage":{"prompt_tokens_details":{"cached_tokens":40}}}),
        ),
        (
            Box::new(OpenAiCompatible::new("sglang", "", None)),
            json!({"choices":[],"usage":{"prompt_tokens_details":{"cached_tokens":40}}}),
        ),
    ];
    for (provider, event) in providers {
        let result = provider
            .transform_stream_chunk("gpt-6-astra", &event.to_string())
            .unwrap()
            .unwrap();
        let value: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(
            value["usage"]["cache_read_tokens"],
            40,
            "{}",
            provider.name()
        );
        assert_eq!(
            value["usage"]["cache_write_tokens"],
            0,
            "{}",
            provider.name()
        );
    }
}

#[test]
fn gemini_usage_only_events_do_not_need_a_candidate_or_invent_a_finish_reason() {
    let p = Gemini::new("test".into());
    let event = json!({"usageMetadata":{"cachedContentTokenCount":40}});
    let chunk = p
        .transform_stream_chunk("gemini-3.8-flash", &event.to_string())
        .unwrap()
        .unwrap();
    let v: Value = serde_json::from_str(&chunk).unwrap();
    assert_eq!(v["usage"]["cache_read_tokens"], 40);
    assert_eq!(v["choices"], json!([]));
}

#[tokio::test]
async fn anthropic_stream_final_usage_preserves_start_counts_without_double_counting() {
    let mut server = mockito::Server::new_async().await;
    let events = [
        json!({"type":"message_start","message":{"id":"a1","usage":{"input_tokens":5,"cache_read_input_tokens":40,"cache_creation_input_tokens":9}}}),
        json!({"type":"message_delta","delta":{},"usage":{"output_tokens":2}}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":7}}),
        json!({"type":"message_stop"}),
    ];
    let body: String = events.iter().map(|e| format!("data: {e}\n\n")).collect();
    let m = server
        .mock("POST", "/messages")
        .with_header("content-type", "text/event-stream")
        .with_body(body)
        .create_async()
        .await;
    let provider = Anthropic::new("test".into()).with_base_url(server.url());
    let client = llmshim::client::ShimClient::new();
    let chunks: Vec<_> = client
        .stream(
            &provider,
            "claude-sonnet-5",
            &json!({"messages":[{"role":"user","content":"hello"}]}),
        )
        .await
        .unwrap()
        .collect()
        .await;
    let last: Value = serde_json::from_str(chunks.last().unwrap().as_ref().unwrap()).unwrap();
    assert_eq!(last["usage"]["cache_read_tokens"], 40);
    assert_eq!(last["usage"]["cache_write_tokens"], 9);
    assert_eq!(last["usage"]["completion_tokens"], 7);
    assert_eq!(last["usage"]["total_tokens"], 61);
    m.assert_async().await;
}

#[tokio::test]
async fn chat_compatible_sse_uses_its_own_wire_parser() {
    for key in ["openrouter", "vllm", "sglang"] {
        let mut server = mockito::Server::new_async().await;
        let event = json!({"choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens_details":{"cached_tokens":40}}});
        let m = server
            .mock("POST", "/chat/completions")
            .with_header("content-type", "text/event-stream")
            .with_body(format!("data: {event}\n\ndata: [DONE]\n\n"))
            .create_async()
            .await;
        let provider: Box<dyn Provider> = if key == "openrouter" {
            Box::new(OpenRouter::new("test".into()).with_base_url(server.url()))
        } else {
            Box::new(OpenAiCompatible::new(key, server.url(), None))
        };
        let chunks: Vec<_> = llmshim::client::ShimClient::new()
            .stream(
                provider.as_ref(),
                "model",
                &json!({"messages":[{"role":"user","content":"hello"}]}),
            )
            .await
            .unwrap()
            .collect()
            .await;
        assert!(!chunks.is_empty(), "{key}");
        let value: Value = serde_json::from_str(chunks[0].as_ref().unwrap()).unwrap();
        assert_eq!(value["usage"]["cache_read_tokens"], 40, "{key}");
        assert_eq!(value["choices"][0]["delta"]["content"], "ok");
        m.assert_async().await;
    }
}

#[tokio::test]
async fn chat_usage_after_finish_arrives_before_the_terminal_marker() {
    let mut server = mockito::Server::new_async().await;
    let first = json!({"choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":"stop"}]});
    let last = json!({"choices":[],"usage":{"prompt_tokens":50,"prompt_tokens_details":{"cached_tokens":40}}});
    let m = server
        .mock("POST", "/chat/completions")
        .with_header("content-type", "text/event-stream")
        .with_body(format!("data: {first}\n\ndata: {last}\n\ndata: [DONE]\n\n"))
        .create_async()
        .await;
    let provider = OpenAiCompatible::new("sglang", server.url(), None);
    let chunks: Vec<_> = llmshim::client::ShimClient::new()
        .stream(
            &provider,
            "model",
            &json!({"messages":[{"role":"user","content":"hello"}]}),
        )
        .await
        .unwrap()
        .collect()
        .await;
    let chunks: Vec<Value> = chunks
        .into_iter()
        .map(|c| serde_json::from_str(&c.unwrap()).unwrap())
        .collect();
    assert!(chunks[0]["choices"][0]["finish_reason"].is_null());
    let terminal = chunks.last().unwrap();
    assert_eq!(terminal["choices"][0]["finish_reason"], "stop");
    assert_eq!(terminal["usage"]["cache_read_tokens"], 40);
    assert_eq!(terminal["choices"][0]["delta"], json!({}));
    m.assert_async().await;
}
