use llmshim::{
    provider::Provider,
    providers::{
        anthropic::Anthropic, gemini::Gemini, openai::OpenAi, openai_compat::OpenAiCompatible,
        openrouter::OpenRouter, xai::Xai,
    },
    schema::{normalize_for, normalize_mcp_tool, normalize_with, Options, Target},
};
use serde_json::{json, Value};

#[test]
fn refs_upgrade_and_sibling_overrides_are_deterministic() {
    let mut schema = json!({"$schema":"http://json-schema.org/draft-07/schema#","definitions":{"Record":{"type":"object","properties":{"name":{"type":"string"}},"description":"base"}},"$ref":"#/definitions/Record","description":"override"});
    let result = normalize_for(Target::Mcp, &mut schema);
    assert!(!result.used_fallback);
    assert_eq!(
        schema["$schema"],
        "https://json-schema.org/draft/2020-12/schema"
    );
    assert_eq!(schema["description"], "override");
    assert_eq!(schema["properties"]["name"]["type"], "string");
    assert!(schema.get("definitions").is_none());
    assert!(schema.get("$ref").is_none());
    let before = schema.clone();
    assert!(!normalize_for(Target::Mcp, &mut schema).changed);
    assert_eq!(before, schema);
}

#[test]
fn schema_keywords_do_not_rewrite_property_names_or_literal_defaults() {
    let literal = json!({"any_of":[1],"type":["not-a-schema"],"default":3});
    let mut schema = json!({"type":"object","properties":{"any_of":{"type":"object","default":literal},"default":{"enum":[literal]}}});
    normalize_for(Target::Mcp, &mut schema);
    assert_eq!(schema["properties"]["any_of"]["default"], literal);
    assert_eq!(schema["properties"]["default"]["enum"][0], literal);
}

#[test]
fn snake_case_wins_and_responses_rewrite_one_of() {
    let mut schema = json!({"type":"object","properties":{"x":{"anyOf":[{"type":"integer"}],"any_of":[{"type":"string"}]},"y":{"oneOf":[{"type":"boolean"},{"type":"string"}]}},"additional_properties":false,"additionalProperties":true});
    normalize_for(Target::OpenAiResponses, &mut schema);
    assert_eq!(schema["properties"]["x"]["anyOf"][0]["type"], "string");
    assert!(schema["properties"]["y"].get("oneOf").is_none());
    assert!(schema["properties"]["y"]["anyOf"].is_array());
    assert_eq!(schema["additionalProperties"], false);
}

#[test]
fn lookarounds_are_removed_without_rewriting_escaped_literals() {
    let mut schema = json!({"type":"object","properties":{"password":{"type":"string","pattern":"^(?=.{8,}$)(?!bad(?=word))[a-z]+$"},"literal":{"type":"string","pattern":r"\(\?=text[(?=)]"}}});
    let r = normalize_for(Target::OpenAiResponses, &mut schema);
    assert!(!r.used_fallback);
    assert_eq!(schema["properties"]["password"]["pattern"], "^[a-z]+$");
    assert!(schema["properties"]["password"]["description"]
        .as_str()
        .unwrap()
        .contains("(?!bad"));
    assert_eq!(
        schema["properties"]["literal"]["pattern"],
        r"\(\?=text[(?=)]"
    );
}

#[test]
fn nullable_modes_and_cloud_combiner_fallback() {
    let original = json!({"type":"object","properties":{"a":{"type":["string","null"]},"b":{"any_of":[{"type":"integer"},{"type":"null"}]}}});
    let mut google = original.clone();
    assert!(!normalize_for(Target::Google, &mut google).used_fallback);
    assert_eq!(google["properties"]["a"]["type"], "string");
    assert_eq!(google["properties"]["a"]["nullable"], true);
    assert_eq!(google["properties"]["b"]["type"], "integer");
    assert_eq!(google["properties"]["b"]["nullable"], true);
    let mut cca = original;
    assert!(!normalize_for(Target::CloudCodeAssist, &mut cca).used_fallback);
    assert!(cca["properties"]["a"].get("nullable").is_none());
    assert_eq!(cca["properties"]["b"]["type"], "integer");
    let mut mixed = json!({"type":"object","properties":{"v":{"anyOf":[{"type":"string"},{"type":"integer"}]}}});
    assert!(normalize_for(Target::CloudCodeAssist, &mut mixed).used_fallback);
    assert_eq!(mixed, json!({"type":"object","properties":{}}));
}

#[test]
fn strict_closes_every_object_and_preserves_optional_meaning_in_nullable_fields() {
    let mut options = Options::for_target(Target::OpenAiResponses);
    options.strict = true;
    let mut schema = json!({"type":"object","properties":{"required":{"type":"string"},"optional":{"type":"integer","default":7,"minimum":1,"description":"Count"},"nested":{"type":"array","items":{"type":"object","properties":{"x":{"type":"boolean"}}}}},"required":["required"]});
    let report = normalize_with(&options, &mut schema);
    assert!(report.strict);
    assert!(!report.used_fallback);
    assert_eq!(schema["additionalProperties"], false);
    assert_eq!(
        schema["required"],
        json!(["nested", "optional", "required"])
    );
    assert_eq!(schema["properties"]["optional"]["anyOf"][1]["type"], "null");
    let description = schema["properties"]["optional"]["description"]
        .as_str()
        .unwrap();
    assert!(description.contains("default: 7"));
    assert!(description.contains("minimum: 1"));
    let item = &schema["properties"]["nested"]["anyOf"][0]["items"];
    assert_eq!(item["additionalProperties"], false);
    assert_eq!(item["required"], json!(["x"]));
    let before = schema.clone();
    assert!(!normalize_with(&options, &mut schema).changed);
    assert_eq!(schema, before);
}

#[test]
fn strict_removes_unsupported_assertions_but_spills_human_constraints() {
    let mut o = Options::for_target(Target::OpenAiChat);
    o.strict = true;
    let mut s = json!({"type":"object","if":{},"then":{},"else":{},"not":{},"unevaluatedProperties":false,"dependentRequired":{"x":["y"]},"patternProperties":{"x":{"type":"string"}},"$dynamicRef":"#x","properties":{"x":{"type":"string","format":"email","pattern":"x+","minLength":1,"maxLength":20,"examples":["x"],"default":"x","contentEncoding":"base64"}},"required":["x"]});
    assert!(normalize_with(&o, &mut s).strict);
    for key in [
        "if",
        "then",
        "else",
        "not",
        "unevaluatedProperties",
        "dependentRequired",
        "patternProperties",
        "$dynamicRef",
    ] {
        assert!(s.get(key).is_none(), "{key}");
    }
    let field = &s["properties"]["x"];
    for key in [
        "format",
        "pattern",
        "minLength",
        "maxLength",
        "examples",
        "default",
        "contentEncoding",
    ] {
        assert!(field.get(key).is_none());
        assert!(field["description"].as_str().unwrap().contains(key));
    }
}

#[test]
fn unresolved_external_recursive_and_invalid_schemas_fall_back() {
    for original in [
        json!({"$ref":"https://unreachable.invalid/schema"}),
        json!({"$defs":{"Node":{"type":"object","properties":{"next":{"$ref":"#/$defs/Node"}}}},"$ref":"#/$defs/Node"}),
        json!({"type":"not-a-type"}),
        json!({"type":"object","required":"bad"}),
        json!({"type":"object","properties":{"x":17}}),
    ] {
        let mut schema = original;
        assert!(normalize_for(Target::OpenAiResponses, &mut schema).used_fallback);
        assert_eq!(schema, json!({"type":"object","properties":{}}));
    }
}

#[test]
fn dereference_expansion_has_a_payload_budget() {
    let mut o = Options::for_target(Target::Mcp);
    o.max_literal_bytes = 1000;
    let mut schema = json!({"$defs":{"Large":{"type":"string","default":"x".repeat(600)}},"type":"object","properties":{"a":{"$ref":"#/$defs/Large"},"b":{"$ref":"#/$defs/Large"}}});
    assert!(normalize_with(&o, &mut schema).used_fallback);
    let mut uri_expansion = json!({"$id":format!("https://schema.test/{}","a".repeat(480)),"type":"object","properties":{"a":{"$anchor":"A","type":"string"},"b":{"$anchor":"B","type":"string"}}});
    assert!(normalize_with(&o, &mut uri_expansion).used_fallback);
}

#[test]
fn draft_tuples_and_dependencies_are_upgraded() {
    let mut schema = json!({"type":"object","properties":{"tuple":{"type":"array","items":[{"type":"string"},{"type":"integer"}],"additionalItems":false}},"dependencies":{"a":["b"],"b":{"properties":{"c":{"type":"boolean"}}}}});
    assert!(!normalize_for(Target::Mcp, &mut schema).used_fallback);
    assert_eq!(
        schema["properties"]["tuple"]["prefixItems"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(schema["properties"]["tuple"]["items"], false);
    assert_eq!(schema["dependentRequired"]["a"], json!(["b"]));
    assert_eq!(
        schema["dependentSchemas"]["b"]["properties"]["c"]["type"],
        "boolean"
    );
}

#[test]
fn public_mcp_ingest_and_native_provider_hooks_normalize_one_bad_tool_only() {
    let mut mcp = json!({"name":"read","inputSchema":{"$defs":{"S":{"type":"string"}},"type":"object","properties":{"path":{"$ref":"#/$defs/S"}}}});
    assert!(!normalize_mcp_tool(&mut mcp).used_fallback);
    assert_eq!(mcp["inputSchema"]["properties"]["path"]["type"], "string");
    let request = json!({"messages":[{"role":"user","content":"hi"}],"tools":[mcp,{"name":"bad","inputSchema":{"$ref":"#/missing"}}]});
    let providers: Vec<Box<dyn Provider>> = vec![
        Box::new(OpenAi::new("test".into())),
        Box::new(Xai::new("test".into())),
        Box::new(Anthropic::new("test".into())),
        Box::new(Gemini::new("test".into())),
        Box::new(OpenRouter::new("test".into())),
        Box::new(OpenAiCompatible::new("custom", "", None)),
    ];
    for p in providers {
        let r = p.transform_request("model", &request).unwrap();
        let schemas: Vec<&Value> = match p.name() {
            "anthropic" => r.body["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| &t["input_schema"])
                .collect(),
            "gemini" => r.body["tools"][0]["functionDeclarations"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| &t["parameters"])
                .collect(),
            "openrouter" | "custom" => r.body["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| &t["function"]["parameters"])
                .collect(),
            _ => r.body["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|t| &t["parameters"])
                .collect(),
        };
        assert_eq!(schemas.len(), 2);
        assert_eq!(
            schemas[0]["properties"]["path"]["type"],
            "string",
            "{}",
            p.name()
        );
        assert_eq!(schemas[1], &json!({"type":"object","properties":{}}));
    }
}

#[test]
fn normalization_applies_after_native_tool_overrides() {
    let p = OpenAi::new("test".into());
    let r=p.transform_request("gpt-6-astra",&json!({"messages":[],"x-openai":{"tools":[{"type":"function","name":"f","strict":true,"parameters":{"type":"object","properties":{"x":{"type":"string","default":"a"}}}}]}})).unwrap();
    assert_eq!(
        r.body["tools"][0]["parameters"]["additionalProperties"],
        false
    );
    assert_eq!(r.body["tools"][0]["parameters"]["required"], json!(["x"]));
}

#[test]
fn explicit_bypass_is_an_exact_schema_noop() {
    let mut o = Options::for_target(Target::Google);
    o.bypass = true;
    let mut schema = json!({"type":["string","null"],"default":null});
    let original = schema.clone();
    assert!(normalize_with(&o, &mut schema).bypassed);
    assert_eq!(schema, original);
}

#[test]
fn environment_bypass_disables_strict_writes() {
    if std::env::var("LLMSHIM_SCHEMA_TEST_CHILD").is_ok() {
        let p = OpenAi::new("test".into());
        let schema =
            json!({"type":"object","properties":{"x":{"type":["string","null"],"default":"a"}}});
        let r=p.transform_request("gpt-6-astra",&json!({"messages":[],"tools":[{"type":"function","function":{"name":"f","strict":true,"parameters":schema}}]})).unwrap();
        assert_eq!(r.body["tools"][0]["strict"], false);
        assert_eq!(r.body["tools"][0]["parameters"], schema);
        return;
    }
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "environment_bypass_disables_strict_writes"])
        .env("LLMSHIM_SCHEMA_TEST_CHILD", "1")
        .env("LLMSHIM_NO_SCHEMA_NORMALIZATION", "1")
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
}

#[test]
fn embedded_resources_anchors_and_uri_encoded_pointers_resolve_without_fetching() {
    let mut schema = json!({"$id":"https://schemas.example.test/root.json","$defs":{
        "resource":{"$id":"child.json","$defs":{"S":{"type":"string"}},"type":"object","properties":{"name":{"$ref":"#/$defs/S"}}},
        "anchored":{"$anchor":"Count","type":"integer"},"a b":{"type":"boolean"}
    },"type":"object","properties":{"child":{"$ref":"child.json"},"count":{"$ref":"#Count"},"flag":{"$ref":"#/$defs/a%20b"}}});
    let result = normalize_for(Target::OpenAiResponses, &mut schema);
    assert!(!result.used_fallback);
    assert_eq!(
        schema["properties"]["child"]["properties"]["name"]["type"],
        "string"
    );
    assert_eq!(schema["properties"]["count"]["type"], "integer");
    assert_eq!(schema["properties"]["flag"]["type"], "boolean");
    assert!(!schema.to_string().contains("$ref"));
    assert!(!schema.to_string().contains("$id"));
}

#[test]
fn duplicate_resource_ids_and_mixed_strict_enums_fall_back() {
    let mut duplicate = json!({"$defs":{"a":{"$id":"https://schema.test/type","type":"string"},"b":{"$id":"https://schema.test/type","type":"number"}},"type":"object"});
    assert!(normalize_for(Target::Mcp, &mut duplicate).used_fallback);
    let mut options = Options::for_target(Target::OpenAiResponses);
    options.strict = true;
    for literal in [json!({"enum":[1,"two",null]}), json!({"const":{"x":1}})] {
        let mut schema = json!({"type":"object","properties":{"x":literal}});
        let r = normalize_with(&options, &mut schema);
        assert!(r.used_fallback);
        assert!(!r.strict);
    }
}

#[test]
fn non_object_output_wrapper_resolves_before_changing_the_document_root() {
    let original =
        json!({"$defs":{"Item":{"type":"string"}},"type":"array","items":{"$ref":"#/$defs/Item"}});
    let output = llmshim::schema::normalize_output_for(Target::OpenAiResponses, &original, true);
    assert!(output.wrapped);
    assert!(output.normalization.strict);
    assert!(!output.normalization.used_fallback);
    assert_eq!(
        output.schema["properties"]["response"]["items"]["type"],
        "string"
    );
    assert_eq!(
        output.unwrap(&json!({"response":["a"]})),
        Some(json!(["a"]))
    );
    assert_eq!(output.unwrap(&json!({"missing":[]})), None);
    let ordinary = llmshim::schema::normalize_output_for(
        Target::OpenAiResponses,
        &json!({"type":"object","properties":{"response":{"type":"string"}},"required":["response"]}),
        true,
    );
    assert!(!ordinary.wrapped);
    assert_eq!(
        ordinary.unwrap(&json!({"response":"a"})),
        Some(json!({"response":"a"}))
    );
}

#[test]
fn invalid_residual_schema_constraints_fall_back_per_tool() {
    for original in [
        json!({"type":"object","minProperties":-1}),
        json!({"type":"object","properties":{"n":{"type":"number","multipleOf":0}}}),
    ] {
        let mut schema = original;
        let report = normalize_for(Target::Mcp, &mut schema);
        assert!(report.used_fallback);
        assert_eq!(schema, json!({"type":"object","properties":{}}));
    }
}

#[test]
fn schema_cache_observes_environment_changes_after_warmup() {
    if std::env::var_os("LLMSHIM_SCHEMA_CACHE_ENV_CHILD").is_some() {
        let input =
            json!({"type":"object","properties":{"optional":{"type":"string","default":"a"}}});
        let mut options = Options::for_target(Target::OpenAiResponses);
        options.strict = true;
        let mut first = input.clone();
        let first_report = normalize_with(&options, &mut first);
        let mut hit = input.clone();
        assert_eq!(normalize_with(&options, &mut hit), first_report);
        assert_eq!(hit, first);
        std::env::set_var("LLMSHIM_NO_STRICT", "1");
        let mut relaxed = input.clone();
        let relaxed_report = normalize_with(&options, &mut relaxed);
        assert!(!relaxed_report.strict);
        assert_ne!(relaxed, first);
        std::env::remove_var("LLMSHIM_NO_STRICT");
        std::env::set_var("LLMSHIM_NO_SCHEMA_NORMALIZATION", "1");
        let mut bypass = input.clone();
        assert!(normalize_with(&options, &mut bypass).bypassed);
        assert_eq!(bypass, input);
        std::env::remove_var("LLMSHIM_NO_SCHEMA_NORMALIZATION");
        std::env::set_var("LLMSHIM_NO_SCHEMA_CACHE", "1");
        let mut uncached = input.clone();
        assert_eq!(normalize_with(&options, &mut uncached), first_report);
        assert_eq!(uncached, first);
        std::env::remove_var("LLMSHIM_NO_SCHEMA_CACHE");
        let mut hit = input.clone();
        assert_eq!(normalize_with(&options, &mut hit), first_report);
        assert_eq!(hit, first);
        return;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "schema_cache_observes_environment_changes_after_warmup",
        ])
        .env("LLMSHIM_SCHEMA_CACHE_ENV_CHILD", "1")
        .env_remove("LLMSHIM_NO_STRICT")
        .env_remove("LLMSHIM_NO_SCHEMA_NORMALIZATION")
        .env_remove("LLMSHIM_NO_SCHEMA_CACHE")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn cached_parameters_do_not_retain_other_tool_metadata() {
    let p = OpenAi::new("test".into());
    let mut request = json!({"messages":[],"tools":[{"type":"function","function":{"name":"first","description":"first description","parameters":{"type":"object","properties":{"value":{"type":"string","description":"first property"}}}}}]});
    p.transform_request("gpt-6-astra", &request).unwrap();
    request["tools"][0]["function"]["name"] = json!("renamed");
    request["tools"][0]["function"]["description"] = json!("new description");
    let result = p.transform_request("gpt-6-astra", &request).unwrap();
    assert_eq!(result.body["tools"][0]["name"], "renamed");
    assert_eq!(result.body["tools"][0]["description"], "new description");
    request["tools"][0]["function"]["parameters"]["properties"]["value"]["description"] =
        json!("new property");
    let result = p.transform_request("gpt-6-astra", &request).unwrap();
    assert_eq!(
        result.body["tools"][0]["parameters"]["properties"]["value"]["description"],
        "new property"
    );
}
