/// Integration test: Gemini with MCP-style tool schemas.
/// Tests that schemars-generated JSON Schema (with $schema, $defs, $ref, nullable types)
/// is correctly sanitized for Gemini's API.
///
/// Run: cargo test --test integration_gemini_tools -- --ignored
use serde_json::json;

fn router() -> llmshim::router::Router {
    llmshim::router::Router::from_env()
}

/// MCP-style tools with all the problematic schema features:
/// $schema, $defs, $ref, "type": ["string", "null"], additionalProperties
fn mcp_tools() -> serde_json::Value {
    json!([
        {
            "type": "function",
            "function": {
                "name": "get_quote",
                "description": "Get stock quotes",
                "parameters": {
                    "$schema": "http://json-schema.org/draft-07/schema#",
                    "type": "object",
                    "properties": {
                        "symbols": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "Ticker symbols"
                        }
                    },
                    "required": ["symbols"],
                    "additionalProperties": false
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "get_options_chain",
                "description": "Get options chain for a symbol",
                "parameters": {
                    "$schema": "http://json-schema.org/draft-07/schema#",
                    "type": "object",
                    "properties": {
                        "symbol": {"type": "string"},
                        "expiration": {"type": ["string", "null"], "description": "Filter by date"},
                        "strike_range_pct": {"type": ["number", "null"]}
                    },
                    "required": ["symbol"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "get_spread_analysis",
                "description": "Analyze an options spread",
                "parameters": {
                    "$schema": "http://json-schema.org/draft-07/schema#",
                    "type": "object",
                    "properties": {
                        "underlying": {"type": "string"},
                        "legs": {
                            "type": "array",
                            "items": {"$ref": "#/$defs/SpreadLeg"}
                        }
                    },
                    "required": ["underlying", "legs"],
                    "$defs": {
                        "SpreadLeg": {
                            "type": "object",
                            "properties": {
                                "symbol": {"type": "string"},
                                "side": {"type": "string"},
                                "quantity": {"type": ["integer", "null"]}
                            },
                            "required": ["symbol", "side"]
                        }
                    }
                }
            }
        }
    ])
}

#[tokio::test]
#[ignore]
async fn gemini_accepts_mcp_tools() {
    let router = router();
    let req = json!({
        "model": "gemini/gemini-3-flash-preview",
        "messages": [{"role": "user", "content": "Get me a quote for AAPL"}],
        "max_tokens": 200,
        "tools": mcp_tools(),
    });

    let result = llmshim::completion(&router, &req).await;
    match &result {
        Ok(resp) => {
            println!("OK: model={}", resp["model"]);
            // Should get a tool call for get_quote
            let msg = &resp["choices"][0]["message"];
            if let Some(tcs) = msg.get("tool_calls").and_then(|t| t.as_array()) {
                println!("Tool calls: {}", serde_json::to_string_pretty(tcs).unwrap());
                assert!(!tcs.is_empty(), "expected at least one tool call");
            } else {
                println!("Content: {}", msg["content"]);
            }
        }
        Err(e) => {
            panic!("Gemini rejected MCP tools: {e}");
        }
    }
}
