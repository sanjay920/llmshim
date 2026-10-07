use llmshim::{
    audio::{TranscriptionModelInfo, TranscriptionRequest},
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
    router::Router,
};
use std::collections::HashSet;

fn router(openai: &str, openrouter: &str) -> Router {
    Router::new()
        .register(
            "openai",
            Box::new(OpenAi::new("oa-key".into()).with_base_url(openai.into())),
        )
        .register(
            "openrouter",
            Box::new(OpenRouter::new("or-key".into()).with_base_url(openrouter.into())),
        )
}

#[test]
fn each_listed_model_is_well_formed_and_labelled_apart() {
    let models = llmshim::transcription_models(&router("http://unused", "http://unused"));
    let mut ids = HashSet::new();
    let mut labels = HashSet::new();
    for m in &models {
        assert_eq!(m.id, format!("{}/{}", m.provider, m.model));
        assert!(!m.model.is_empty() && m.model.trim() == m.model, "{m:?}");
        assert!(ids.insert(&m.id), "duplicate id {}", m.id);
        assert!(labels.insert(&m.label), "duplicate label {}", m.label);
        let suffix = match m.provider.as_str() {
            "openai" => " (OpenAI)",
            "openrouter" => " via OpenRouter)",
            other => panic!("unexpected provider {other}"),
        };
        assert!(m.label.ends_with(suffix), "{m:?}");
        assert!(
            !m.label.starts_with(' ') && !m.label.contains("( "),
            "{m:?}"
        );
    }
    // OpenAI lists exactly the catalog rows that gate its requests.
    let openai: Vec<_> = models
        .iter()
        .filter(|m| m.provider == "openai")
        .map(|m| m.model.as_str())
        .collect();
    let catalog: Vec<_> = llmshim::catalog::audio::TRANSCRIPTION_MODELS
        .iter()
        .map(|row| row.model)
        .collect();
    assert_eq!(openai, catalog);
    // The same model through two providers has two ids a person can tell apart.
    let whisper: Vec<_> = models
        .iter()
        .filter(|m| m.id.ends_with("whisper-1"))
        .map(|m| (m.id.as_str(), m.label.as_str()))
        .collect();
    assert_eq!(
        whisper,
        [
            ("openai/whisper-1", "Whisper (OpenAI)"),
            (
                "openrouter/openai/whisper-1",
                "Whisper 1 (OpenAI via OpenRouter)"
            ),
        ]
    );
    // Every OpenRouter slug keeps its vendor prefix.
    assert!(models
        .iter()
        .filter(|m| m.provider == "openrouter")
        .all(|m| m
            .model
            .split_once('/')
            .is_some_and(|(v, n)| !v.is_empty() && !n.is_empty())));
}

/// Sends one transcription for `model` and expects the bare model on the wire
/// at the server registered for its provider.
async fn routes_to(
    server: &mut mockito::ServerGuard,
    router: &Router,
    model: &TranscriptionModelInfo,
    key: &str,
) {
    let mock = server
        .mock("POST", "/audio/transcriptions")
        .match_header("authorization", format!("Bearer {key}").as_str())
        .match_body(mockito::Matcher::Regex(format!(
            "name=\"model\"\\r\\n\\r\\n{}\\r\\n",
            regex_escape(&model.model)
        )))
        .with_header("content-type", "application/json")
        .with_body(r#"{"text":"Ada speaks."}"#)
        .expect(1)
        .create_async()
        .await;
    let request = TranscriptionRequest::new(&model.id, vec![0, 255, 1], "clip.wav", "audio/wav");
    let response = llmshim::transcription(router, &request)
        .await
        .unwrap_or_else(|e| panic!("{} did not route: {e}", model.id));
    assert_eq!(response.text, "Ada speaks.");
    mock.assert_async().await;
    mock.remove_async().await;
}

fn regex_escape(text: &str) -> String {
    text.chars()
        .flat_map(|c| {
            let escape = !c.is_ascii_alphanumeric() && c != '/' && c != '-';
            escape.then_some('\\').into_iter().chain([c])
        })
        .collect()
}

#[tokio::test]
async fn every_listed_id_routes_to_its_providers_transcription_request() {
    let mut openai = mockito::Server::new_async().await;
    let mut openrouter = mockito::Server::new_async().await;
    let router = router(&openai.url(), &openrouter.url());
    let models = llmshim::transcription_models(&router);
    assert!(models.iter().any(|m| m.provider == "openai"));
    assert!(models.iter().any(|m| m.provider == "openrouter"));
    for model in &models {
        match model.provider.as_str() {
            "openai" => routes_to(&mut openai, &router, model, "oa-key").await,
            _ => routes_to(&mut openrouter, &router, model, "or-key").await,
        }
    }
}

#[tokio::test]
async fn ids_carry_the_key_a_provider_is_registered_under() {
    let mut server = mockito::Server::new_async().await;
    let router = Router::new().register(
        "speech-eu",
        Box::new(OpenRouter::new("or-key".into()).with_base_url(server.url())),
    );
    let models = llmshim::transcription_models(&router);
    let whisper = models
        .iter()
        .find(|m| m.model == "openai/whisper-1")
        .unwrap();
    assert_eq!(whisper.id, "speech-eu/openai/whisper-1");
    assert_eq!(whisper.provider, "speech-eu");
    routes_to(&mut server, &router, whisper, "or-key").await;
    // The provider alone keys its list by its own name.
    let own = OpenRouter::new("or-key".into()).transcription_models();
    assert!(own.iter().any(|m| m.id == "openrouter/openai/whisper-1"));
}

#[test]
fn a_provider_without_transcription_lists_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let auth = ChatGptAuth::new(dir.path().join("auth.json"));
    let silent: Vec<(&str, Box<dyn Provider>)> = vec![
        ("anthropic", Box::new(Anthropic::new("test".into()))),
        ("chatgpt", Box::new(ChatGpt::new(auth))),
        ("gemini", Box::new(Gemini::new("test".into()))),
        (
            "vllm",
            Box::new(OpenAiCompatible::new("vllm", "http://localhost", None)),
        ),
        ("xai", Box::new(Xai::new("test".into()))),
    ];
    let request = TranscriptionRequest::new("m", vec![1], "clip.wav", "audio/wav");
    let mut router = Router::new();
    for (key, provider) in silent {
        assert!(provider.transcription_models().is_empty(), "{key}");
        // Listing nothing agrees with refusing locally.
        assert!(
            provider.transcription_request("m", &request).is_err(),
            "{key}"
        );
        router = router.register(key, provider);
    }
    assert!(llmshim::transcription_models(&router).is_empty());
    assert!(llmshim::transcription_models(&Router::new()).is_empty());
}
