use llmshim::provider::Provider;
use llmshim::providers::openrouter::OpenRouter;
use serde_json::{json, Value};

fn provider() -> OpenRouter {
    OpenRouter::new("test-key-abc".into())
}

// ============================================================
// transform_request
// ============================================================

#[test]
fn request_url_and_bearer_auth() {
    let p = provider();
    let req = json!({
        "model": "anthropic/claude-sonnet-4.5",
        "messages": [{"role": "user", "content": "hi"}],
    });
    let result = p
        .transform_request("anthropic/claude-sonnet-4.5", &req)
        .unwrap();
    assert_eq!(result.url, "https://openrouter.ai/api/v1/chat/completions");
    let auth = result.headers.iter().find(|(k, _)| k == "Authorization");
    assert_eq!(auth.unwrap().1, "Bearer test-key-abc");
}

#[test]
fn request_preserves_slug_and_messages() {
    // The vendor/model slug (with its internal slash) is passed straight through.
    let p = provider();
    let req = json!({
        "model": "meta-llama/llama-3.1-70b-instruct",
        "messages": [{"role": "user", "content": "hi"}],
    });
    let result = p
        .transform_request("meta-llama/llama-3.1-70b-instruct", &req)
        .unwrap();
    assert_eq!(result.body["model"], "meta-llama/llama-3.1-70b-instruct");
    assert_eq!(result.body["messages"][0]["content"], "hi");
}

#[test]
fn request_forwards_tools_unchanged() {
    // OpenRouter is Chat Completions-native, so nested tools need no translation.
    let p = provider();
    let tools = json!([{
        "type": "function",
        "function": {
            "name": "get_weather",
            "description": "Get weather",
            "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}
        }
    }]);
    let req = json!({
        "model": "openai/gpt-5.1",
        "messages": [{"role": "user", "content": "hi"}],
        "tools": tools,
        "tool_choice": "auto",
        "response_format": {"type": "json_object"},
    });
    let result = p.transform_request("openai/gpt-5.1", &req).unwrap();
    assert_eq!(result.body["tools"], tools);
    assert_eq!(result.body["tool_choice"], "auto");
    assert_eq!(result.body["response_format"]["type"], "json_object");
}

#[test]
fn request_maps_reasoning_effort_1to1() {
    let p = provider();
    for effort in ["low", "medium", "high", "xhigh", "max", "none"] {
        let req = json!({
            "model": "x/y",
            "messages": [{"role": "user", "content": "hi"}],
            "reasoning_effort": effort,
        });
        let result = p.transform_request("x/y", &req).unwrap();
        assert_eq!(
            result.body["reasoning"]["effort"], effort,
            "effort {effort} should map 1:1 (OpenRouter vocab is a superset)"
        );
    }
}

#[test]
fn request_reasoning_mode_pro_bumps_one_tier() {
    let p = provider();
    let req = json!({
        "model": "x/y",
        "messages": [{"role": "user", "content": "hi"}],
        "reasoning_effort": "high",
        "reasoning_mode": "pro",
    });
    let result = p.transform_request("x/y", &req).unwrap();
    assert_eq!(result.body["reasoning"]["effort"], "xhigh");
}

#[test]
fn request_native_reasoning_wins_over_effort() {
    let p = provider();
    let req = json!({
        "model": "x/y",
        "messages": [{"role": "user", "content": "hi"}],
        "reasoning_effort": "high",
        "x-openrouter": {"reasoning": {"max_tokens": 2000}},
    });
    let result = p.transform_request("x/y", &req).unwrap();
    assert_eq!(result.body["reasoning"]["max_tokens"], 2000);
    // The unified effort mapping is bypassed when native reasoning is supplied.
    assert!(result.body["reasoning"].get("effort").is_none());
}

#[test]
fn request_disables_middle_out_by_default() {
    let p = provider();
    let req = json!({
        "model": "x/y",
        "messages": [{"role": "user", "content": "hi"}],
    });
    let result = p.transform_request("x/y", &req).unwrap();
    assert_eq!(result.body["transforms"], json!([]));
}

#[test]
fn request_transforms_opt_in_preserved() {
    let p = provider();
    let req = json!({
        "model": "x/y",
        "messages": [{"role": "user", "content": "hi"}],
        "x-openrouter": {"transforms": ["middle-out"]},
    });
    let result = p.transform_request("x/y", &req).unwrap();
    assert_eq!(result.body["transforms"], json!(["middle-out"]));
}

#[test]
fn request_x_openrouter_body_passthrough() {
    let p = provider();
    let req = json!({
        "model": "x/y",
        "messages": [{"role": "user", "content": "hi"}],
        "x-openrouter": {
            "provider": {"order": ["anthropic", "openai"], "allow_fallbacks": false},
            "models": ["anthropic/claude-sonnet-4.5", "openai/gpt-5.1"],
            "route": "fallback"
        },
    });
    let result = p.transform_request("x/y", &req).unwrap();
    assert_eq!(result.body["provider"]["order"][0], "anthropic");
    assert_eq!(result.body["provider"]["allow_fallbacks"], false);
    assert_eq!(result.body["models"][1], "openai/gpt-5.1");
    assert_eq!(result.body["route"], "fallback");
}

#[test]
fn request_x_openrouter_attribution_headers_not_in_body() {
    let p = provider();
    let req = json!({
        "model": "x/y",
        "messages": [{"role": "user", "content": "hi"}],
        "x-openrouter": {"http_referer": "https://example.com", "x_title": "MyApp"},
    });
    let result = p.transform_request("x/y", &req).unwrap();
    let referer = result.headers.iter().find(|(k, _)| k == "HTTP-Referer");
    let title = result.headers.iter().find(|(k, _)| k == "X-Title");
    assert_eq!(referer.unwrap().1, "https://example.com");
    assert_eq!(title.unwrap().1, "MyApp");
    // Attribution controls must not leak into the request body.
    assert!(result.body.get("http_referer").is_none());
    assert!(result.body.get("x_title").is_none());
}

#[test]
fn request_sanitizes_foreign_reasoning_fields() {
    let p = provider();
    let req = json!({
        "model": "x/y",
        "messages": [{
            "role": "assistant",
            "content": "hi",
            "reasoning_content": "x",
            "reasoning_signature": "anthropic-sig-should-not-leak",
            "redacted_reasoning_content": "redacted-should-not-leak"
        }]
    });
    let result = p.transform_request("x/y", &req).unwrap();
    let body = serde_json::to_string(&result.body).unwrap();
    assert!(!body.contains("anthropic-sig-should-not-leak"));
    assert!(!body.contains("redacted-should-not-leak"));
    assert!(!body.contains("reasoning_signature"));
}

#[test]
fn request_preserves_vision_and_tool_messages() {
    let p = provider();
    let req = json!({
        "model": "x/y",
        "messages": [
            {"role": "user", "content": [
                {"type": "text", "text": "what is this?"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}
            ]},
            {"role": "assistant", "content": "", "tool_calls": [{
                "id": "call_1", "type": "function",
                "function": {"name": "f", "arguments": "{}"}
            }]},
            {"role": "tool", "tool_call_id": "call_1", "content": "result"}
        ]
    });
    let result = p.transform_request("x/y", &req).unwrap();
    let msgs = result.body["messages"].as_array().unwrap();
    // Image block preserved in OpenAI form.
    assert_eq!(msgs[0]["content"][1]["type"], "image_url");
    // tool_calls stay in Chat Completions shape (no function_call splitting).
    assert_eq!(msgs[1]["tool_calls"][0]["id"], "call_1");
    // role:"tool" stays as-is.
    assert_eq!(msgs[2]["role"], "tool");
    assert_eq!(msgs[2]["tool_call_id"], "call_1");
}

// ============================================================
// transform_response
// ============================================================

#[test]
fn response_normalizes_reasoning_to_reasoning_content() {
    let p = provider();
    let resp = json!({
        "id": "gen_1",
        "model": "anthropic/claude-sonnet-4.5",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "42", "reasoning": "let me think..."},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 5, "completion_tokens": 2, "total_tokens": 7}
    });
    let result = p
        .transform_response("anthropic/claude-sonnet-4.5", resp)
        .unwrap();
    let msg = &result["choices"][0]["message"];
    assert_eq!(msg["reasoning"][0]["text"], "let me think...");
    assert_eq!(msg["content"], "42");
    assert_eq!(result["usage"]["total_tokens"], 7);
}

#[test]
fn response_passes_tool_calls_through() {
    let p = provider();
    let resp = json!({
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": null, "tool_calls": [{
                "id": "call_9", "type": "function",
                "function": {"name": "search", "arguments": "{\"q\":\"x\"}"}
            }]},
            "finish_reason": "tool_calls"
        }]
    });
    let result = p.transform_response("x/y", resp).unwrap();
    let tc = &result["choices"][0]["message"]["tool_calls"][0];
    assert_eq!(tc["wire_ids"][0]["id"], "call_9");
    assert_eq!(tc["function"]["name"], "search");
    assert_eq!(result["choices"][0]["finish_reason"], "tool_calls");
}

#[test]
fn response_error_object_becomes_error() {
    let p = provider();
    let resp = json!({"error": {"code": 402, "message": "Insufficient credits"}});
    let result = p.transform_response("x/y", resp);
    assert!(result.is_err());
}

// ============================================================
// transform_stream_chunk
// ============================================================

#[test]
fn stream_normalizes_reasoning_delta() {
    let p = provider();
    let chunk = json!({
        "choices": [{"index": 0, "delta": {"reasoning": "thinking"}, "finish_reason": null}]
    });
    let result = p
        .transform_stream_chunk("x/y", &serde_json::to_string(&chunk).unwrap())
        .unwrap()
        .unwrap();
    let parsed: Value = serde_json::from_str(&result).unwrap();
    assert_eq!(
        parsed["choices"][0]["delta"]["reasoning"][0]["text"],
        "thinking"
    );
}

#[test]
fn stream_passes_content_delta() {
    let p = provider();
    let chunk = json!({
        "choices": [{"index": 0, "delta": {"content": "Hello"}, "finish_reason": null}]
    });
    let result = p
        .transform_stream_chunk("x/y", &serde_json::to_string(&chunk).unwrap())
        .unwrap()
        .unwrap();
    let parsed: Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["choices"][0]["delta"]["content"], "Hello");
}

#[test]
fn stream_skips_unparseable() {
    let p = provider();
    let result = p.transform_stream_chunk("x/y", "not json").unwrap();
    assert!(result.is_none());
}

// ============================================================
// Accounting — MOH-228
//
// OpenRouter reports what it actually charged for a generation, and which
// upstream served it. Both were being thrown away: the request never asked for
// the cost, and the catalog estimate overwrote it when it arrived anyway.
//
// The two fixtures are real captured responses from
// `openrouter/deepseek/deepseek-v4.1-flash` with `usage: {include: true}`.
// ============================================================

fn captured_response() -> Value {
    serde_json::from_str(include_str!("fixtures/openrouter-usage-include.json")).unwrap()
}

/// The `data:` payloads of the captured stream, `[DONE]` excluded.
fn captured_stream() -> Vec<String> {
    include_str!("fixtures/openrouter-usage-include-stream.sse")
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|data| *data != "[DONE]")
        .map(str::to_owned)
        .collect()
}

#[test]
fn request_never_injects_an_accounting_parameter() {
    // Measured 2026-09-22: OpenRouter returns `usage.cost` unconditionally —
    // a stream carries it with no `stream_options`, and `include: false` does
    // not suppress it. Its docs call both parameters deprecated and without
    // effect. There is therefore nothing to ask for, and injecting a no-op
    // into every request would be a passthrough inventing a parameter.
    let result = provider()
        .transform_request(
            "deepseek/deepseek-v4.1-flash",
            &json!({"messages": [{"role": "user", "content": "hi"}]}),
        )
        .unwrap();
    assert!(
        result.body.get("usage").is_none(),
        "nothing asked for accounting, so nothing should be sent: {}",
        result.body
    );
}

#[test]
fn request_forwards_an_explicit_usage_object_unchanged() {
    // A caller may still be pointing at an OpenRouter-compatible endpoint that
    // does honour the switch. Dropping their value would be the passthrough
    // bug this adapter exists to avoid — `usage` was missing from the
    // forwarded key list entirely.
    let p = provider();
    for asked in [json!({"include": false}), json!({"include": true})] {
        let result = p
            .transform_request(
                "deepseek/deepseek-v4.1-flash",
                &json!({"messages": [{"role": "user", "content": "hi"}], "usage": asked}),
            )
            .unwrap();
        assert_eq!(result.body["usage"], asked);
    }

    // `x-openrouter.usage` is the same statement in the native namespace.
    let result = p
        .transform_request(
            "deepseek/deepseek-v4.1-flash",
            &json!({
                "messages": [{"role": "user", "content": "hi"}],
                "x-openrouter": {"usage": {"include": true}},
            }),
        )
        .unwrap();
    assert_eq!(result.body["usage"], json!({"include": true}));
}

#[test]
fn response_keeps_the_reported_bill_and_who_served_the_call() {
    // `normalize_response` adds cache counters to the usage object in place, so
    // the accounting fields beside them have to survive it — and `provider` is
    // the only record of which upstream OpenRouter routed to.
    let result = provider()
        .transform_response("deepseek/deepseek-v4.1-flash", captured_response())
        .unwrap();

    assert_eq!(result["provider"], "Together");
    assert_eq!(result["id"], "gen-1790055505-SQhAOovddGuTBgM1SM6y");

    let usage = &result["usage"];
    assert_eq!(usage["cost"], 0.0000873);
    assert_eq!(usage["is_byok"], false);
    assert_eq!(usage["cost_details"]["upstream_inference_cost"], 0.0000873);
    assert_eq!(
        usage["cost_details"]["upstream_inference_prompt_cost"],
        0.0000141
    );
    // The normalized counters are computed beside them, not instead of them.
    assert_eq!(usage["cache_read_tokens"], 0);
    assert_eq!(usage["uncached_input_tokens"], 47);
    assert_eq!(usage["completion_tokens_details"]["reasoning_tokens"], 52);
}

#[test]
fn the_reported_bill_outranks_the_catalog_estimate() {
    // A catalog price is an estimate of this number, and deliberately an upper
    // bound. Letting it overwrite the number would discard the only exact
    // figure in the response.
    let mut result = provider()
        .transform_response("deepseek/deepseek-v4.1-flash", captured_response())
        .unwrap();
    llmshim::cost::stamp("openrouter", "deepseek/deepseek-v4.1-flash", &mut result);

    assert_eq!(result["usage"]["cost_usd"], 0.0000873);
    assert_eq!(result["usage"]["cost_source"], "provider");
    assert_eq!(
        result["usage"]["cost_usd"], result["usage"]["cost"],
        "the stamped cost must be the reported bill, to the last decimal"
    );
}

#[test]
fn a_reported_bill_is_believed_even_where_the_catalog_has_no_price() {
    // The whole point for an aggregator: OpenRouter carries slugs the catalog
    // has never heard of, and for those the reported bill is the *only*
    // possible answer. A catalog miss must not null it out.
    let mut result = json!({"usage": {
        "prompt_tokens": 47, "completion_tokens": 61, "uncached_input_tokens": 47,
        "cost": 0.0000873,
    }});
    llmshim::cost::stamp("openrouter", "definitely-not-in-the-catalog", &mut result);
    assert_eq!(result["usage"]["cost_usd"], 0.0000873);
    assert_eq!(result["usage"]["cost_source"], "provider");
}

#[test]
fn a_provider_that_reports_nothing_still_gets_the_catalog_path() {
    // Every other provider. The old behaviour, unchanged, now labelled.
    let mut result = json!({"usage": {
        "uncached_input_tokens": 1_000_000, "completion_tokens": 0, "prompt_tokens": 1_000_000,
    }});
    llmshim::cost::stamp("anthropic", "claude-sonnet-5", &mut result);
    assert!(
        result["usage"]["cost_usd"].as_f64().unwrap() > 0.0,
        "a catalogued model must still be priced"
    );
    assert_eq!(result["usage"]["cost_source"], "catalog");

    // And an unpriceable model still stamps null, never 0 — the source names
    // the path that answered, not the confidence of the answer.
    let mut unknown = json!({"usage": {"uncached_input_tokens": 1, "completion_tokens": 1}});
    llmshim::cost::stamp("nowhere", "definitely-not-a-model", &mut unknown);
    assert!(unknown["usage"]["cost_usd"].is_null());
    assert_eq!(unknown["usage"]["cost_source"], "catalog");
}

#[test]
fn stream_chunks_keep_the_bill_and_the_serving_upstream() {
    // The client stamps every chunk as it passes (`stamp_chunk`), so the
    // terminal chunk is where cost lands on a stream. `provider` rides on all
    // of them.
    let p = provider();
    let chunks = captured_stream();
    assert!(
        chunks.len() > 2,
        "fixture should be a real multi-chunk stream"
    );

    let mut terminal = None;
    for chunk in &chunks {
        let out = p
            .transform_stream_chunk("deepseek/deepseek-v4.1-flash", chunk)
            .unwrap()
            .expect("openrouter chunks are forwarded, not swallowed");
        let out = llmshim::cost::stamp_chunk("openrouter", "deepseek/deepseek-v4.1-flash", out);
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["provider"], "Together");
        assert_eq!(parsed["id"], "gen-1790055506-E2fSyv9tIeHD6SXuq9A3");
        if parsed["usage"].is_object() {
            terminal = Some(parsed);
        }
    }

    let terminal = terminal.expect("the captured stream carries usage on its last chunk");
    assert_eq!(terminal["usage"]["cost"], 0.0001734);
    assert_eq!(terminal["usage"]["cost_usd"], 0.0001734);
    assert_eq!(terminal["usage"]["cost_source"], "provider");
    assert_eq!(terminal["usage"]["is_byok"], false);
    assert_eq!(
        terminal["usage"]["cost_details"]["upstream_inference_completions_cost"],
        0.0001572
    );
    assert_eq!(terminal["usage"]["cache_read_tokens"], 0);
}

#[tokio::test]
async fn a_buffered_response_still_names_the_upstream_that_served_it() {
    // `shim::collect` rebuilds one response out of the chunks for every
    // buffered path (managed tool protocols, structured output). It copied a
    // fixed field list that omitted `provider`, so the aggregator's statement
    // of who served the call was dropped exactly where a repair might need it.
    let chunks: Vec<_> = captured_stream().into_iter().map(Ok).collect();
    let collected = llmshim::shim::collect(Box::pin(futures::stream::iter(chunks)))
        .await
        .unwrap();

    assert_eq!(collected["provider"], "Together");
    assert_eq!(collected["id"], "gen-1790055506-E2fSyv9tIeHD6SXuq9A3");
    assert_eq!(collected["usage"]["cost"], 0.0001734);
}

#[test]
fn a_log_entry_says_which_of_the_two_numbers_it_carries() {
    // Reading spend off a JSONL log means knowing whether the figure is an
    // invoice or an estimate.
    let mut result = provider()
        .transform_response("deepseek/deepseek-v4.1-flash", captured_response())
        .unwrap();
    llmshim::cost::stamp("openrouter", "deepseek/deepseek-v4.1-flash", &mut result);
    let entry = llmshim::log::LogEntry::from_response(
        "openrouter",
        "deepseek/deepseek-v4.1-flash",
        &result,
        std::time::Duration::from_millis(1),
    );
    assert_eq!(entry.cost_usd, Some(0.0000873));
    assert_eq!(entry.cost_source.as_deref(), Some("provider"));
    assert_eq!(
        entry.request_id.as_deref(),
        Some("gen-1790055505-SQhAOovddGuTBgM1SM6y")
    );
}
