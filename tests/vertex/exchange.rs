use super::*;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use std::sync::atomic::{AtomicUsize, Ordering};
struct Endpoint(Arc<AtomicUsize>);
impl TokenExchange for Endpoint {
    fn exchange(&self, request: reqwest::Request) -> ExchangeFuture<'_> {
        Box::pin(async move {
            self.0.fetch_add(1, Ordering::SeqCst);
            assert_eq!(
                request.url().as_str(),
                "https://oauth2.googleapis.com/token"
            );
            assert_eq!(request.method(), "POST");
            let body = std::str::from_utf8(request.body().unwrap().as_bytes().unwrap()).unwrap();
            assert!(
                body.contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Ajwt-bearer")
            );
            let assertion = body.split("assertion=").nth(1).unwrap();
            let pieces: Vec<_> = assertion.split('.').collect();
            assert_eq!(pieces.len(), 3);
            let header: Value =
                serde_json::from_slice(&URL_SAFE_NO_PAD.decode(pieces[0]).unwrap()).unwrap();
            let claims: Value =
                serde_json::from_slice(&URL_SAFE_NO_PAD.decode(pieces[1]).unwrap()).unwrap();
            assert_eq!(header["alg"], "RS256");
            assert_eq!(claims["iss"], "fixture@example.invalid");
            assert_eq!(claims["aud"], "https://oauth2.googleapis.com/token");
            assert_eq!(
                claims["scope"],
                "https://www.googleapis.com/auth/cloud-platform"
            );
            assert_eq!(
                claims["exp"].as_u64().unwrap() - claims["iat"].as_u64().unwrap(),
                3600
            );
            let public = base64::engine::general_purpose::STANDARD
                .decode(
                    include_str!("../fixtures/vertex/synthetic-public-key.b64")
                        .split_whitespace()
                        .collect::<String>(),
                )
                .unwrap();
            ring::signature::UnparsedPublicKey::new(
                &ring::signature::RSA_PKCS1_2048_8192_SHA256,
                public,
            )
            .verify(
                format!("{}.{}", pieces[0], pieces[1]).as_bytes(),
                &URL_SAFE_NO_PAD.decode(pieces[2]).unwrap(),
            )
            .unwrap();
            Ok(ExchangeResponse::new(
                200,
                serde_json::to_vec(&json!({
                "access_token":"fixture-token","token_type":"Bearer","expires_in":120}))
                .unwrap(),
            ))
        })
    }
}
#[tokio::test(start_paused = true)]
async fn fake_token_endpoint_refreshes_service_account_with_single_flight_and_fake_clock() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("identity.json");
    std::fs::write(
        &path,
        serde_json::to_vec(&json!({"type":"service_account",
        "client_email":"fixture@example.invalid","private_key_id":"fixture",
        "private_key":include_str!("../fixtures/vertex/synthetic-key.pem"),
        "token_uri":"https://oauth2.googleapis.com/token"}))
        .unwrap(),
    )
    .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let source = ServiceAccount::new(path).with_exchange(Arc::new(Endpoint(calls.clone())));
    let provider = Vertex::gemini(GoogleIdentity::token_source(
        "fixture-project",
        "global",
        "selected-file",
        Arc::new(source),
    ));
    let prepared = provider
        .transform_request(
            "model",
            &json!({
        "messages":[{"role":"user","content":"Hi"}]}),
        )
        .unwrap();
    let authorize = || async {
        let mut request =
            reqwest::Request::new(reqwest::Method::POST, prepared.url.parse().unwrap());
        provider
            .transport()
            .unwrap()
            .authorize(&mut request)
            .await
            .unwrap();
        assert_eq!(request.headers()["authorization"], "Bearer fixture-token");
    };
    futures::future::join_all((0..12).map(|_| authorize())).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    tokio::time::advance(Duration::from_secs(91)).await;
    authorize().await;
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while calls.load(Ordering::SeqCst) < 2 && std::time::Instant::now() < deadline {
        tokio::task::yield_now().await;
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

struct Response(u16, Value);
impl TokenExchange for Response {
    fn exchange(&self, _: reqwest::Request) -> ExchangeFuture<'_> {
        Box::pin(async move {
            Ok(ExchangeResponse::new(
                self.0,
                serde_json::to_vec(&self.1).unwrap(),
            ))
        })
    }
}
#[tokio::test]
async fn token_endpoint_status_and_shape_fail_closed_without_leaking_response_text() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("identity.json");
    std::fs::write(
        &path,
        serde_json::to_vec(&json!({"type":"service_account",
        "client_email":"fixture@example.invalid","private_key_id":"fixture",
        "private_key":include_str!("../fixtures/vertex/synthetic-key.pem"),
        "token_uri":"https://oauth2.googleapis.com/token"}))
        .unwrap(),
    )
    .unwrap();
    for (status, response, code, expected_status) in [
        (
            400,
            json!({"error":"invalid_grant","message":"secret-fixture"}),
            "token_rejected",
            401,
        ),
        (
            503,
            json!({"message":"secret-fixture"}),
            "token_service_unavailable",
            503,
        ),
        (
            200,
            json!({"access_token":"secret-fixture","expires_in":0,"token_type":"Bearer"}),
            "invalid_token",
            401,
        ),
        (
            200,
            json!({"access_token":"secret-fixture","expires_in":120,"token_type":"Other"}),
            "invalid_token",
            401,
        ),
    ] {
        let source =
            ServiceAccount::new(path.clone()).with_exchange(Arc::new(Response(status, response)));
        let provider = Vertex::gemini(GoogleIdentity::token_source(
            "fixture-project",
            "global",
            "fixture",
            Arc::new(source),
        ));
        let req = provider
            .transform_request(
                "model",
                &json!({
            "messages":[{"role":"user","content":"Hi"}]}),
            )
            .unwrap();
        let mut request = reqwest::Request::new(reqwest::Method::POST, req.url.parse().unwrap());
        let error = provider
            .transport()
            .unwrap()
            .authorize(&mut request)
            .await
            .unwrap_err();
        let classified = VertexError::from_shim(&error).unwrap();
        assert_eq!(classified.code, code);
        assert_eq!(classified.status, expected_status);
        assert!(!error.to_string().contains("secret-fixture"));
        assert!(request.headers().get("authorization").is_none());
    }
}

#[tokio::test]
async fn service_account_type_and_token_uri_never_select_an_alternate_source() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("identity.json");
    for (kind, uri) in [
        ("authorized_user", "https://oauth2.googleapis.com/token"),
        ("service_account", "https://example.invalid/token"),
    ] {
        std::fs::write(
            &path,
            serde_json::to_vec(&json!({"type":kind,
            "client_email":"fixture@example.invalid",
            "private_key":include_str!("../fixtures/vertex/synthetic-key.pem"),
            "token_uri":uri}))
            .unwrap(),
        )
        .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let source =
            ServiceAccount::new(path.clone()).with_exchange(Arc::new(Endpoint(calls.clone())));
        let error = source.token().await.err().unwrap();
        assert!(matches!(error, CredentialError::InvalidServiceAccount));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}
