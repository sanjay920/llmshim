use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
struct Refresh(Arc<AtomicUsize>);
impl TokenSource for Refresh {
    fn token(&self) -> TokenFuture<'_> {
        Box::pin(async {
            self.0.fetch_add(1, Ordering::SeqCst);
            tokio::task::yield_now().await;
            AccessToken::new("fixture-token", Duration::from_secs(120))
        })
    }
}
#[tokio::test(start_paused = true)]
async fn cache_single_flight_and_refresh_before_expiry() {
    let calls = Arc::new(AtomicUsize::new(0));
    let identity = GoogleIdentity::token_source(
        "fixture-project",
        "global",
        "selected",
        Arc::new(Refresh(calls.clone())),
    );
    let provider = Vertex::gemini(identity);
    let req = provider
        .transform_request(
            "model",
            &json!({
        "messages":[{"role":"user","content":"Hi"}]}),
        )
        .unwrap();
    let authorize = || async {
        let mut request = reqwest::Client::new().post(&req.url).build().unwrap();
        provider
            .transport()
            .unwrap()
            .authorize(&mut request)
            .await
            .unwrap();
        assert!(request.headers()["authorization"].is_sensitive());
    };
    futures::future::join_all((0..20).map(|_| authorize())).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    tokio::time::advance(Duration::from_secs(91)).await;
    authorize().await;
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while calls.load(Ordering::SeqCst) < 2 && std::time::Instant::now() < deadline {
        tokio::task::yield_now().await;
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}
#[tokio::test]
async fn failing_named_file_never_falls_back_and_wrong_origin_never_gets_a_token() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Vertex::gemini(GoogleIdentity::service_account(
        "fixture-project",
        "global",
        "selected-file",
        dir.path().join("missing.json"),
    ));
    let req = provider
        .transform_request(
            "model",
            &json!({
        "messages":[{"role":"user","content":"Hi"}]}),
        )
        .unwrap();
    let mut request = reqwest::Client::new().post(req.url).build().unwrap();
    let error = provider
        .transport()
        .unwrap()
        .authorize(&mut request)
        .await
        .unwrap_err();
    assert_eq!(VertexError::from_shim(&error).unwrap().status, 401);
    assert!(request.headers().get("authorization").is_none());
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = Vertex::gemini(GoogleIdentity::token_source(
        "fixture-project",
        "global",
        "chosen",
        Arc::new(Refresh(calls.clone())),
    ));
    let mut request = reqwest::Client::new()
        .post("https://example.invalid")
        .build()
        .unwrap();
    assert!(provider
        .transport()
        .unwrap()
        .authorize(&mut request)
        .await
        .is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[test]
fn hostile_environment_cannot_replace_a_missing_named_identity() {
    const CHILD: &str = "LLMSHIM_VERTEX_FIXTURE_CHILD";
    if std::env::var_os(CHILD).is_some() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let provider = Vertex::gemini(GoogleIdentity::service_account(
                "fixture-project",
                "global",
                "selected-file",
                "missing-vertex-fixture.json".into(),
            ));
            let req = provider
                .transform_request(
                    "model",
                    &json!({
                "messages":[{"role":"user","content":"Hi"}]}),
                )
                .unwrap();
            let mut request =
                reqwest::Request::new(reqwest::Method::POST, req.url.parse().unwrap());
            let error = provider
                .transport()
                .unwrap()
                .authorize(&mut request)
                .await
                .unwrap_err();
            assert_eq!(
                VertexError::from_shim(&error).unwrap().code,
                "identity_file_unavailable"
            );
            assert!(request.headers().get("authorization").is_none());
        });
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let ambient = dir.path().join("ambient.json");
    std::fs::write(
        &ambient,
        r#"{"type":"authorized_user","refresh_token":"ambient-fixture"}"#,
    )
    .unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "credentials::hostile_environment_cannot_replace_a_missing_named_identity",
        ])
        .env(CHILD, "1")
        .env("GOOGLE_APPLICATION_CREDENTIALS", ambient)
        .env("GOOGLE_OAUTH_ACCESS_TOKEN", "ambient-fixture")
        .env("GCE_METADATA_HOST", "metadata.invalid")
        .env("CLOUDSDK_AUTH_ACCESS_TOKEN", "ambient-fixture")
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
            break;
        }
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("named identity child timed out");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

struct Pending;
impl TokenSource for Pending {
    fn token(&self) -> TokenFuture<'_> {
        Box::pin(std::future::pending())
    }
}
#[tokio::test(start_paused = true)]
async fn selected_token_source_timeout_is_typed_and_never_adds_a_header() {
    let provider = Vertex::gemini(GoogleIdentity::token_source(
        "fixture-project",
        "global",
        "selected",
        Arc::new(Pending),
    ));
    let req = provider
        .transform_request(
            "model",
            &json!({
        "messages":[{"role":"user","content":"Hi"}]}),
        )
        .unwrap();
    let mut request = reqwest::Request::new(reqwest::Method::POST, req.url.parse().unwrap());
    let started = tokio::time::Instant::now();
    let transport = provider.transport().unwrap();
    let error = tokio::time::timeout(Duration::from_secs(60), transport.authorize(&mut request))
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(started.elapsed(), Duration::from_secs(30));
    let classified = VertexError::from_shim(&error).unwrap();
    assert_eq!(classified.code, "token_timeout");
    assert_eq!(classified.status, 503);
    assert!(request.headers().get("authorization").is_none());
}
