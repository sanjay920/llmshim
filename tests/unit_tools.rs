/// Tests for tool/function calling format translation across all providers.
///
/// All providers accept tools in OpenAI Chat Completions format:
///   {"type": "function", "function": {"name": "...", "description": "...", "parameters": {...}}}
///
/// Each provider translates to its native format:
///   OpenAI (Responses API): {"type": "function", "name": "...", "parameters": {...}}
///   xAI (Responses API):    same as OpenAI
///   Anthropic:               {"name": "...", "description": "...", "input_schema": {...}}
///   Gemini:                  {"functionDeclarations": [{"name": "...", "parameters": {...}}]}
use llmshim::provider::Provider;
use serde_json::{json, Value};

fn chat_completions_tools() -> Value {
    json!([{
        "type": "function",
        "function": {
            "name": "get_weather",
            "description": "Get current weather",
            "parameters": {
                "type": "object",
                "properties": {
                    "city": {"type": "string", "description": "City name"}
                },
                "required": ["city"]
            }
        }
    }])
}

fn flat_tools() -> Value {
    json!([{
        "type": "function",
        "name": "get_weather",
        "description": "Get current weather",
        "parameters": {
            "type": "object",
            "properties": {
                "city": {"type": "string", "description": "City name"}
            },
            "required": ["city"]
        }
    }])
}

// ============================================================
// OpenAI: Chat Completions format → Responses API flat format
// ============================================================

#[test]
fn openai_translates_nested_tools_to_flat() {
    let p = llmshim::providers::openai::OpenAi::new("k".into());
    let req = json!({
        "model": "gpt-5.4",
        "messages": [{"role": "user", "content": "weather?"}],
        "tools": chat_completions_tools(),
    });
    let result = p.transform_request("gpt-5.4", &req).unwrap();
    let tools = result.body["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1);
    // Should be flat: name at top level, no nested "function" object
    assert_eq!(tools[0]["name"], "get_weather");
    assert_eq!(tools[0]["type"], "function");
    assert!(
        tools[0].get("function").is_none(),
        "should not have nested function"
    );
    assert_eq!(
        tools[0]["parameters"]["properties"]["city"]["type"],
        "string"
    );
}

#[test]
fn openai_passes_through_flat_tools() {
    let p = llmshim::providers::openai::OpenAi::new("k".into());
    let req = json!({
        "model": "gpt-5.4",
        "messages": [{"role": "user", "content": "weather?"}],
        "tools": flat_tools(),
    });
    let result = p.transform_request("gpt-5.4", &req).unwrap();
    let tools = result.body["tools"].as_array().unwrap();
    assert_eq!(tools[0]["name"], "get_weather");
    assert!(tools[0].get("function").is_none());
}

#[test]
fn openai_translates_multiple_tools() {
    let p = llmshim::providers::openai::OpenAi::new("k".into());
    let req = json!({
        "model": "gpt-5.4",
        "messages": [{"role": "user", "content": "hi"}],
        "tools": [
            {"type": "function", "function": {"name": "tool_a", "description": "A", "parameters": {"type": "object"}}},
            {"type": "function", "function": {"name": "tool_b", "description": "B", "parameters": {"type": "object"}}},
        ],
    });
    let result = p.transform_request("gpt-5.4", &req).unwrap();
    let tools = result.body["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0]["name"], "tool_a");
    assert_eq!(tools[1]["name"], "tool_b");
}

// ============================================================
// xAI: Chat Completions format → Responses API flat format
// ============================================================

#[test]
fn xai_translates_nested_tools_to_flat() {
    let p = llmshim::providers::xai::Xai::new("k".into());
    let req = json!({
        "model": "grok-4-1-fast-reasoning",
        "messages": [{"role": "user", "content": "weather?"}],
        "tools": chat_completions_tools(),
    });
    let result = p
        .transform_request("grok-4-1-fast-reasoning", &req)
        .unwrap();
    let tools = result.body["tools"].as_array().unwrap();
    assert_eq!(tools[0]["name"], "get_weather");
    assert_eq!(tools[0]["type"], "function");
    assert!(
        tools[0].get("function").is_none(),
        "should not have nested function"
    );
}

#[test]
fn xai_passes_through_flat_tools() {
    let p = llmshim::providers::xai::Xai::new("k".into());
    let req = json!({
        "model": "grok-4-1-fast-reasoning",
        "messages": [{"role": "user", "content": "weather?"}],
        "tools": flat_tools(),
    });
    let result = p
        .transform_request("grok-4-1-fast-reasoning", &req)
        .unwrap();
    let tools = result.body["tools"].as_array().unwrap();
    assert_eq!(tools[0]["name"], "get_weather");
}

// ============================================================
// Anthropic: Chat Completions format → Anthropic format
// ============================================================

#[test]
fn anthropic_translates_nested_tools() {
    let p = llmshim::providers::anthropic::Anthropic::new("k".into());
    let req = json!({
        "model": "claude-sonnet-4-6",
        "messages": [{"role": "user", "content": "weather?"}],
        "tools": chat_completions_tools(),
        "max_tokens": 100,
    });
    let result = p.transform_request("claude-sonnet-4-6", &req).unwrap();
    let tools = result.body["tools"].as_array().unwrap();
    assert_eq!(tools[0]["name"], "get_weather");
    assert_eq!(tools[0]["description"], "Get current weather");
    assert!(
        tools[0].get("input_schema").is_some(),
        "Anthropic uses input_schema"
    );
}

// ============================================================
// Gemini: Chat Completions format → functionDeclarations
// ============================================================

#[test]
fn gemini_translates_nested_tools() {
    let p = llmshim::providers::gemini::Gemini::new("k".into());
    let req = json!({
        "model": "gemini-3-flash-preview",
        "messages": [{"role": "user", "content": "weather?"}],
        "tools": chat_completions_tools(),
    });
    let result = p.transform_request("gemini-3-flash-preview", &req).unwrap();
    let decls = &result.body["tools"][0]["functionDeclarations"];
    assert_eq!(decls[0]["name"], "get_weather");
    assert_eq!(decls[0]["description"], "Get current weather");
}

#[test]
fn gemini_handles_flat_tools() {
    let p = llmshim::providers::gemini::Gemini::new("k".into());
    let req = json!({
        "model": "gemini-3-flash-preview",
        "messages": [{"role": "user", "content": "weather?"}],
        "tools": flat_tools(),
    });
    let result = p.transform_request("gemini-3-flash-preview", &req).unwrap();
    let decls = &result.body["tools"][0]["functionDeclarations"];
    assert_eq!(decls[0]["name"], "get_weather");
}

#[test]
fn gemini_strips_schema_and_defs() {
    let p = llmshim::providers::gemini::Gemini::new("k".into());
    let req = json!({
        "model": "gemini-3-flash-preview",
        "messages": [{"role": "user", "content": "hi"}],
        "tools": [{
            "type": "function",
            "function": {
                "name": "analyze_spread",
                "description": "Analyze a spread",
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
        }],
    });
    let result = p.transform_request("gemini-3-flash-preview", &req).unwrap();
    let params = &result.body["tools"][0]["functionDeclarations"][0]["parameters"];

    // $schema stripped
    assert!(
        params.get("$schema").is_none(),
        "$schema should be stripped"
    );
    // $defs stripped
    assert!(params.get("$defs").is_none(), "$defs should be stripped");
    // $ref resolved: legs.items should be inlined SpreadLeg object
    let items = &params["properties"]["legs"]["items"];
    assert_eq!(items["type"], "object");
    assert_eq!(items["properties"]["symbol"]["type"], "string");
    // type array converted to single type
    assert_eq!(items["properties"]["quantity"]["type"], "integer");
}

#[test]
fn gemini_converts_nullable_type_arrays() {
    let p = llmshim::providers::gemini::Gemini::new("k".into());
    let req = json!({
        "model": "gemini-3-flash-preview",
        "messages": [{"role": "user", "content": "hi"}],
        "tools": [{
            "type": "function",
            "function": {
                "name": "search",
                "description": "Search",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string"},
                        "limit": {"type": ["integer", "null"]},
                        "filter": {"type": ["string", "null"]},
                        "score": {"type": ["number", "null"]}
                    }
                }
            }
        }],
    });
    let result = p.transform_request("gemini-3-flash-preview", &req).unwrap();
    let props = &result.body["tools"][0]["functionDeclarations"][0]["parameters"]["properties"];
    assert_eq!(props["query"]["type"], "string");
    assert_eq!(props["limit"]["type"], "integer");
    assert_eq!(props["filter"]["type"], "string");
    assert_eq!(props["score"]["type"], "number");
}

#[test]
fn gemini_handles_mcp_style_tools_with_all_issues() {
    let p = llmshim::providers::gemini::Gemini::new("k".into());
    // Simulate what schemars generates for an MCP tool with Option fields
    let req = json!({
        "model": "gemini-3-flash-preview",
        "messages": [{"role": "user", "content": "hi"}],
        "tools": [{
            "type": "function",
            "function": {
                "name": "get_unusual_activity",
                "description": "Get unusual market activity",
                "parameters": {
                    "$schema": "http://json-schema.org/draft-07/schema#",
                    "type": "object",
                    "properties": {
                        "watchlist": {"type": ["string", "null"], "description": "Watchlist name"},
                        "min_iv_rank": {"type": ["number", "null"], "description": "Min IV rank"},
                        "limit": {"type": ["integer", "null"], "description": "Max results"}
                    },
                    "additionalProperties": false
                }
            }
        }],
    });
    let result = p.transform_request("gemini-3-flash-preview", &req).unwrap();
    let params = &result.body["tools"][0]["functionDeclarations"][0]["parameters"];
    assert!(params.get("$schema").is_none());
    assert!(params.get("additionalProperties").is_none());
    assert_eq!(params["properties"]["watchlist"]["type"], "string");
    assert_eq!(params["properties"]["min_iv_rank"]["type"], "number");
    assert_eq!(params["properties"]["limit"]["type"], "integer");
}

// ============================================================
// Tool call result messages in conversation history
// ============================================================

#[test]
fn openai_handles_tool_result_in_history() {
    let p = llmshim::providers::openai::OpenAi::new("k".into());
    let req = json!({
        "model": "gpt-5.4",
        "messages": [
            {"role": "user", "content": "weather in tokyo?"},
            {"role": "assistant", "content": null, "tool_calls": [{
                "id": "call_1", "type": "function",
                "function": {"name": "get_weather", "arguments": "{\"city\":\"Tokyo\"}"}
            }]},
            {"role": "tool", "tool_call_id": "call_1", "content": "Sunny, 25C"},
            {"role": "assistant", "content": "It's sunny in Tokyo!"},
            {"role": "user", "content": "thanks"},
        ],
        "tools": chat_completions_tools(),
    });
    let result = p.transform_request("gpt-5.4", &req).unwrap();
    // Should compile without error — tools and tool messages both present
    assert!(result.body.get("input").is_some());
    assert!(result.body.get("tools").is_some());
}

#[test]
fn xai_handles_tool_result_in_history() {
    let p = llmshim::providers::xai::Xai::new("k".into());
    let req = json!({
        "model": "grok-4-1-fast-reasoning",
        "messages": [
            {"role": "user", "content": "weather?"},
            {"role": "assistant", "content": null, "tool_calls": [{
                "id": "call_1", "type": "function",
                "function": {"name": "get_weather", "arguments": "{\"city\":\"Tokyo\"}"}
            }]},
            {"role": "tool", "tool_call_id": "call_1", "content": "Sunny"},
        ],
        "tools": chat_completions_tools(),
    });
    let result = p
        .transform_request("grok-4-1-fast-reasoning", &req)
        .unwrap();
    assert!(result.body.get("tools").is_some());
    let tools = result.body["tools"].as_array().unwrap();
    assert_eq!(tools[0]["name"], "get_weather");
}

#[test]
fn anthropic_handles_tool_calls_in_history() {
    let p = llmshim::providers::anthropic::Anthropic::new("k".into());
    let req = json!({
        "model": "claude-sonnet-4-6",
        "messages": [
            {"role": "user", "content": "weather?"},
            {"role": "assistant", "content": null, "tool_calls": [{
                "id": "call_1", "type": "function",
                "function": {"name": "get_weather", "arguments": "{\"city\":\"Tokyo\"}"}
            }]},
            {"role": "tool", "tool_call_id": "call_1", "content": "Sunny"},
            {"role": "user", "content": "thanks"},
        ],
        "tools": chat_completions_tools(),
        "max_tokens": 100,
    });
    let result = p.transform_request("claude-sonnet-4-6", &req).unwrap();
    let messages = result.body["messages"].as_array().unwrap();
    // tool_calls should be converted to tool_use blocks
    assert_eq!(messages[1]["content"][0]["type"], "tool_use");
    // tool result should be converted to tool_result
    assert_eq!(messages[2]["content"][0]["type"], "tool_result");
}
