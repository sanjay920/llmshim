use super::*;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Mutex,
};
struct Endpoint {
    calls: Arc<AtomicUsize>,
    audiences: Arc<Mutex<Vec<Audience>>>,
    responses: Vec<u16>,
}
impl TokenSource for Endpoint {
    fn token(&self, audience: Audience) -> TokenFuture<'_> {
        Box::pin(async move {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            self.audiences.lock().unwrap().push(audience);
            match self.responses.get(n).copied().unwrap_or(200) {
                0 => return std::future::pending().await,
                503 => return Err(CredentialError::Unavailable),
                401 => return Err(CredentialError::Rejected),
                _ => {}
            }
            AccessToken::new(&format!("fixture-{n}"), Duration::from_secs(120))
        })
    }
}
async fn header(provider: &Azure) -> reqwest::header::HeaderValue {
    try_header(provider).await.unwrap()
}
async fn try_header(provider: &Azure) -> llmshim::error::Result<reqwest::header::HeaderValue> {
    let prepared = provider
        .transform_request("chat-deployment", &json!({"messages":[]}))
        .unwrap();
    let mut request = reqwest::Request::new(reqwest::Method::POST, prepared.url.parse().unwrap());
    provider
        .transport()
        .unwrap()
        .authorize(&mut request)
        .await?;
    let name = if request.headers().contains_key("api-key") {
        "api-key"
    } else {
        "authorization"
    };
    let header = request.headers()[name].clone();
    assert!(header.is_sensitive());
    Ok(header)
}
#[tokio::test(start_paused = true)]
async fn both_audiences_are_explicit_and_rejected_tokens_refresh_selected_source() {
    for (audience, scope, host) in [
        (
            Audience::CognitiveServices,
            "https://cognitiveservices.azure.com/.default",
            "https://fixture.openai.azure.com",
        ),
        (
            Audience::Foundry,
            "https://ai.azure.com/.default",
            "https://fixture.services.ai.azure.com",
        ),
    ] {
        assert_eq!(audience.scope(), scope);
        let calls = Arc::new(AtomicUsize::new(0));
        let audiences = Arc::new(Mutex::new(Vec::new()));
        let provider = Azure::new(
            host,
            AzureIdentity::entra(
                "selected",
                audience,
                Arc::new(Endpoint {
                    calls: calls.clone(),
                    audiences: audiences.clone(),
                    responses: vec![],
                }),
            )
            .unwrap(),
        )
        .unwrap()
        .with_deployment("chat-deployment", "gpt-6-sol")
        .unwrap();
        let tokens = futures::future::join_all((0..20).map(|_| header(&provider))).await;
        assert!(tokens.iter().all(|h| h == "Bearer fixture-0"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(provider
            .transport()
            .unwrap()
            .authorization_rejected(
                401,
                r#"{"error":{"code":"Unauthorized"}}"#,
                Some(&tokens[0])
            )
            .await
            .unwrap());
        assert_eq!(header(&provider).await, "Bearer fixture-1");
        assert!(audiences.lock().unwrap().iter().all(|a| *a == audience));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
    assert_eq!(header(&provider()).await, "test-key");
}

fn selected(responses: Vec<u16>) -> (Azure, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let identity = AzureIdentity::entra(
        "selected",
        Audience::CognitiveServices,
        Arc::new(Endpoint {
            calls: calls.clone(),
            audiences: Default::default(),
            responses,
        }),
    )
    .unwrap();
    (
        Azure::new("https://fixture.openai.azure.com", identity)
            .unwrap()
            .with_deployment("chat-deployment", "gpt-6-sol")
            .unwrap(),
        calls,
    )
}
#[path = "refresh.rs"]
mod refresh;

#[tokio::test]
async fn endpoints_are_closed_and_never_receive_foreign_credentials() {
    for endpoint in [
        "http://fixture.openai.azure.com",
        "https://key@fixture.openai.azure.com",
        "https://fixture.openai.azure.com?key=fixture",
        "https://fixture.openai.azure.com/other",
        "https://fixture.openai.azure.com/#fragment",
    ] {
        assert!(Azure::new(
            endpoint,
            AzureIdentity::api_key("fixture", "test-key").unwrap()
        )
        .is_err());
    }
    let (provider, calls) = selected(vec![]);
    for (method, url) in [
        (
            reqwest::Method::GET,
            "https://fixture.openai.azure.com/openai/v1/chat/completions",
        ),
        (
            reqwest::Method::POST,
            "https://other.invalid/openai/v1/chat/completions",
        ),
        (
            reqwest::Method::POST,
            "https://fixture.openai.azure.com/openai/v1/chat/completions?key=x",
        ),
    ] {
        let mut request = reqwest::Request::new(method, url.parse().unwrap());
        assert!(provider
            .transport()
            .unwrap()
            .authorize(&mut request)
            .await
            .is_err());
        assert!(request.headers().get("authorization").is_none());
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let keyed = super::provider();
    let mut request = reqwest::Request::new(
        reqwest::Method::POST,
        "https://fixture.openai.azure.com/openai/v1/chat/completions"
            .parse()
            .unwrap(),
    );
    request.headers_mut().insert(
        "api-key",
        reqwest::header::HeaderValue::from_static("ambient"),
    );
    request.headers_mut().insert(
        "authorization",
        reqwest::header::HeaderValue::from_static("Bearer ambient"),
    );
    keyed
        .transport()
        .unwrap()
        .authorize(&mut request)
        .await
        .unwrap();
    assert!(!request.headers().contains_key("authorization"));
    assert_eq!(request.headers()["api-key"], "test-key");
    assert!(request.headers()["api-key"].is_sensitive());
}
#[test]
fn named_binding_failure_never_uses_hostile_ambient_credentials() {
    const CHILD: &str = "LLMSHIM_AZURE_FIXTURE_CHILD";
    if std::env::var_os(CHILD).is_some() {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let (provider, calls) = selected(vec![401]);
            let error = try_header(&provider).await.unwrap_err();
            assert_eq!(AzureError::from_shim(&error).unwrap().code, "TokenRejected");
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        });
        return;
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "credentials::named_binding_failure_never_uses_hostile_ambient_credentials",
        ])
        .env(CHILD, "1")
        .env("AZURE_OPENAI_API_KEY", "ambient-fixture")
        .env("OPENAI_API_KEY", "ambient-fixture")
        .env("AZURE_CLIENT_SECRET", "ambient-fixture")
        .env("AZURE_AUTHORITY_HOST", "https://authority.invalid")
        .env("IDENTITY_ENDPOINT", "https://metadata.invalid")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            let output = child.wait_with_output().unwrap();
            assert!(
                status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("named-binding child stalled");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
