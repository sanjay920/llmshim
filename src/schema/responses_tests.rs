use super::*;
use crate::{provider::Provider, providers::openai::OpenAi};

#[test]
fn native_search_and_namespace_schemas_share_the_request_budget() {
    let tool = json!({"type": "tool_search", "execution": "client",
        "description": "Find a tool", "parameters": {"type": "object"}});
    let namespace = json!({"type": "namespace", "name": "functions", "tools": [
        {"type": "function", "name": "read", "parameters": {"type": "object"}},
    ]});
    for tool in [tool, namespace] {
        for (key, count) in [
            ("x-responses-tools", 1),
            ("x-responses-tools", 2),
            ("x-responses-loaded-tools", 1),
            ("x-responses-loaded-tools", 2),
        ] {
            let request = json!({key: vec![tool.clone(); count]});
            let mut budget = RequestBudget::with_limits(BudgetLimits {
                schema_copies: 1,
                ..BudgetLimits::default()
            });
            let result = budget.reserve_request_schemas(&request);
            assert_eq!(result.is_ok(), count == 1);
            if let Err(error) = result {
                assert!(error.to_string().contains("schema budget exceeded"));
            }
        }
    }
}

#[test]
fn search_parameters_use_native_schema_normalization() {
    let request = json!({"messages": [{"role": "user", "content": "hello"}],
    "x-responses-tools": [{"type": "tool_search", "execution": "client",
        "description": "Find a tool", "parameters": {"type": "object", "properties": {
            "query": {"oneOf": [{"type": "string"}, {"type": "integer"}]},
        }}}]});
    let sent = OpenAi::new("key".into())
        .transform_request("test", &request)
        .unwrap();
    let query = &sent.body["tools"][0]["parameters"]["properties"]["query"];
    assert!(query["anyOf"].is_array());
    assert!(query.get("oneOf").is_none());
}
