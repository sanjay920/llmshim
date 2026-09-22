/// Integration tests for OpenRouter hitting the real API.
/// Run with: OPENROUTER_API_KEY=... cargo test --test integration_openrouter -- --ignored
/// Override the chat model with OR_TEST_MODEL. The default is a `:free` model so
/// the suite runs on an account without purchased credits — but free models are
/// rate-limited, so add `--test-threads=1` to avoid parallel free-tier 429s.
use serde_json::{json, Value};

fn router() -> llmshim::router::Router {
    llmshim::router::Router::from_env()
}

fn test_model() -> String {
    std::env::var("OR_TEST_MODEL")
        .unwrap_or_else(|_| "openrouter/poolside/laguna-s-2.1:free".into())
}

/// Run a completion, or skip the test (returning None) when OpenRouter reports
/// the account can't pay for the model (402) — so the suite passes on both free
/// and funded accounts.
async fn complete_or_skip(router: &llmshim::router::Router, req: &Value) -> Option<Value> {
    match llmshim::completion(router, req).await {
        Ok(v) => Some(v),
        Err(e) => {
            let s = e.to_string();
            if s.contains("402") || s.contains("Insufficient credits") {
                eprintln!("SKIP: OpenRouter account lacks credits for this model ({s})");
                None
            } else {
                panic!("completion failed: {e}");
            }
        }
    }
}

#[tokio::test]
#[ignore]
async fn openrouter_basic_completion() {
    if std::env::var("OPENROUTER_API_KEY").is_err() {
        return;
    }
    let router = router();
    let req = json!({
        "model": test_model(),
        "messages": [{"role": "user", "content": "In one short sentence, what is Rust?"}],
        "max_tokens": 2000,
    });
    let Some(resp) = complete_or_skip(&router, &req).await else {
        return;
    };
    assert_eq!(resp["object"], "chat.completion");
    let content = resp["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or("");
    assert!(!content.is_empty(), "expected a response, got: {resp}");
    println!("model={} | said: {content}", resp["model"]);
}

#[tokio::test]
#[ignore]
async fn openrouter_reasoning() {
    if std::env::var("OPENROUTER_API_KEY").is_err() {
        return;
    }
    let router = router();
    let req = json!({
        "model": test_model(),
        "messages": [{"role": "user", "content": "In one sentence, why is the sky blue? Reason briefly first."}],
        "max_tokens": 2000,
        "reasoning_effort": "high",
    });
    let Some(resp) = complete_or_skip(&router, &req).await else {
        return;
    };
    let msg = &resp["choices"][0]["message"];
    // A reasoning model's reasoning is normalized into reasoning_content.
    let reasoning = msg["reasoning"][0]["text"].as_str().unwrap_or("");
    let content = msg["content"].as_str().unwrap_or("");
    assert!(
        !reasoning.is_empty() || !content.is_empty(),
        "expected reasoning or answer, got: {resp}"
    );
    println!(
        "reasoning chars={}, answer chars={}",
        reasoning.len(),
        content.len()
    );
}

#[tokio::test]
#[ignore]
async fn openrouter_tool_call() {
    if std::env::var("OPENROUTER_API_KEY").is_err() {
        return;
    }
    let router = router();
    // A widely-available tool-capable model; skipped on accounts without credits.
    let model =
        std::env::var("OR_TOOL_MODEL").unwrap_or_else(|_| "openrouter/openai/gpt-4o-mini".into());
    let req = json!({
        "model": model,
        "messages": [{"role": "user", "content": "What's the weather in Tokyo? Use the tool."}],
        "max_tokens": 200,
        "tools": [{
            "type": "function",
            "function": {
                "name": "get_weather",
                "description": "Get the weather for a city",
                "parameters": {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]}
            }
        }]
    });
    let Some(resp) = complete_or_skip(&router, &req).await else {
        return;
    };
    let tool_calls = resp["choices"][0]["message"].get("tool_calls");
    assert!(tool_calls.is_some(), "expected a tool call, got: {resp}");
    println!("tool call ok");
}

/// MOH-228 receipt: the response carries OpenRouter's own bill, llmshim stamps
/// that in preference to the catalog estimate, and both numbers are printed
/// side by side so the difference between an invoice and an estimate is a
/// measurement rather than a claim.
///
/// Pinned to one model the owner authorized for live calls. Run with:
/// `cargo test --test integration_openrouter -- --ignored --nocapture
///  openrouter_reports_its_own_cost`
const ACCOUNTING_MODEL: &str = "openrouter/deepseek/deepseek-v4.1-flash";

/// Print what a usage object says about money, and assert the invariant: when
/// the provider reported a bill, that is the number llmshim stamped.
fn report(label: &str, body: &Value) {
    let usage = &body["usage"];
    let reported = usage["cost"].as_f64();
    let estimate = llmshim::cost::cost_usd(
        "openrouter",
        ACCOUNTING_MODEL.trim_start_matches("openrouter/"),
        usage,
    );

    println!("--- {label} ---");
    println!("  provider (served by) : {}", body["provider"]);
    println!("  id (generation)      : {}", body["id"]);
    println!("  usage.cost (reported): {reported:?}");
    println!("  usage.cost_usd       : {}", usage["cost_usd"]);
    println!("  usage.cost_source    : {}", usage["cost_source"]);
    println!("  catalog estimate     : {estimate:?}");
    println!("  cost_details         : {}", usage["cost_details"]);
    println!("  is_byok              : {}", usage["is_byok"]);
    println!(
        "  tokens p/c/total     : {} / {} / {}",
        usage["prompt_tokens"], usage["completion_tokens"], usage["total_tokens"]
    );

    assert!(
        body["provider"].is_string(),
        "an aggregator must say which upstream served the call"
    );
    let reported = reported.expect("accounting is on by default, so a bill must come back");
    assert_eq!(usage["cost_source"], "provider");
    assert_eq!(
        usage["cost_usd"].as_f64(),
        Some(reported),
        "the stamped cost must be the reported bill, not the catalog estimate"
    );
}

#[tokio::test]
#[ignore]
async fn openrouter_reports_its_own_cost() {
    if std::env::var("OPENROUTER_API_KEY").is_err() {
        return;
    }
    let router = router();

    let Some(resp) = complete_or_skip(
        &router,
        &json!({
            "model": ACCOUNTING_MODEL,
            "messages": [{"role": "user", "content": "Reply with one short sentence: what is 2+2?"}],
            "max_tokens": 2000,
        }),
    )
    .await
    else {
        return;
    };
    report("non-stream", &resp);

    // A stream only carries usage when the caller asks for the terminal usage
    // chunk; llmshim deliberately does not default that. The cost rides on it.
    use futures::StreamExt;
    let mut stream = llmshim::stream(
        &router,
        &json!({
            "model": ACCOUNTING_MODEL,
            "messages": [{"role": "user", "content": "Reply with one short sentence: what is 3+3?"}],
            "max_tokens": 2000,
            "stream_options": {"include_usage": true},
        }),
    )
    .await
    .expect("stream should open");

    let mut terminal = None;
    while let Some(chunk) = stream.next().await {
        let parsed: Value = serde_json::from_str(&chunk.expect("chunk")).unwrap();
        if parsed["usage"].is_object() {
            terminal = Some(parsed);
        }
    }
    report(
        "stream (terminal chunk)",
        &terminal.expect("include_usage must produce a terminal usage chunk"),
    );
}
