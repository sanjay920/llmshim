use llmshim::{
    provider::Provider,
    providers::{
        anthropic::Anthropic, gemini::Gemini, openai::OpenAi, openai_compat::OpenAiCompatible,
        openrouter::OpenRouter, xai::Xai,
    },
};

#[test]
fn documented_defaults_require_the_native_endpoint() {
    let providers: Vec<Box<dyn Provider>> = vec![
        Box::new(OpenAi::new("test-key".into())),
        Box::new(Anthropic::new("test-key".into())),
        Box::new(Gemini::new("test-key".into())),
        Box::new(Xai::new("test-key".into())),
    ];
    for provider in providers {
        assert!(
            provider.uses_documented_model_defaults(),
            "{}",
            provider.name()
        );
    }
    for endpoint in [
        "https://gateway.example/v1",
        "https://api.openai.com.evil.example/v1",
        "https://api.anthropic.com/v2",
        "http://generativelanguage.googleapis.com/v1beta",
        "https://api.x.ai/v1?route=custom",
    ] {
        let providers: Vec<Box<dyn Provider>> = vec![
            Box::new(OpenAi::new("test-key".into()).with_base_url(endpoint.into())),
            Box::new(Anthropic::new("test-key".into()).with_base_url(endpoint.into())),
            Box::new(Gemini::new("test-key".into()).with_base_url(endpoint.into())),
            Box::new(Xai::new("test-key".into()).with_base_url(endpoint.into())),
        ];
        for provider in providers {
            assert!(
                !provider.uses_documented_model_defaults(),
                "{} at {endpoint}",
                provider.name()
            );
        }
    }
    for provider in [
        Box::new(OpenRouter::new("test-key".into())) as Box<dyn Provider>,
        Box::new(OpenAiCompatible::new(
            "openai",
            "https://api.openai.com/v1",
            None,
        )),
    ] {
        assert!(
            !provider.uses_documented_model_defaults(),
            "{}",
            provider.name()
        );
    }
}
