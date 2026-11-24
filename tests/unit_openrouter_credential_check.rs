//! OpenRouter can authenticate a key without choosing a model or sending a prompt.

use llmshim::{client::ShimClient, error::ShimError, providers::openrouter::OpenRouter};

const KEY: &str = "synthetic-openrouter-key";
const VALID: &str =
    r#"{"data":{"label":"synthetic-test-key","usage":0,"is_management_key":false}}"#;

fn provider(url: String) -> OpenRouter {
    OpenRouter::new(KEY.into()).with_base_url(url)
}

#[tokio::test]
async fn the_key_check_uses_the_authenticated_key_endpoint_and_never_a_model() {
    let mut server = mockito::Server::new_async().await;
    let key = server
        .mock("GET", "/key")
        .match_header("authorization", format!("Bearer {KEY}").as_str())
        .match_body(mockito::Matcher::Exact(String::new()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(VALID)
        .expect(1)
        .create_async()
        .await;
    let model = server
        .mock("POST", mockito::Matcher::Any)
        .expect(0)
        .create_async()
        .await;

    let checked = ShimClient::new()
        .credential_check(&provider(server.url()))
        .await
        .expect("the authenticated key response is valid");

    assert!(checked, "OpenRouter has a credential check");
    key.assert_async().await;
    model.assert_async().await;
}

#[tokio::test]
async fn an_empty_success_object_is_not_proof_of_a_key() {
    let mut server = mockito::Server::new_async().await;
    server
        .mock("GET", "/key")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"data":{}}"#)
        .expect(1)
        .create_async()
        .await;

    let error = ShimClient::new()
        .credential_check(&provider(server.url()))
        .await
        .expect_err("empty key data must not pass");

    assert!(matches!(
        error,
        ShimError::ProviderError { status: 502, .. }
    ));
}

#[tokio::test]
async fn an_error_cannot_echo_the_bearer_key_to_the_caller() {
    let mut server = mockito::Server::new_async().await;
    server
        .mock("GET", "/key")
        .with_status(401)
        .with_header("content-type", "application/json")
        .with_body(format!(
            r#"{{"error":{{"message":"refused {KEY}","label":"secret-key-metadata"}}}}"#
        ))
        .expect(1)
        .create_async()
        .await;

    let error = ShimClient::new()
        .credential_check(&provider(server.url()))
        .await
        .expect_err("the key is refused");

    match error {
        ShimError::ProviderError { status, body, .. } => {
            assert_eq!(status, 401);
            assert_eq!(body, "the provider rejected the credential");
            assert!(!body.contains(KEY), "the error echoed the key: {body}");
            assert!(!body.contains("secret-key-metadata"));
        }
        other => panic!("expected a provider error, got {other:?}"),
    }
}

#[tokio::test]
async fn an_oversized_error_keeps_its_status_and_retry_delay() {
    let mut server = mockito::Server::new_async().await;
    server
        .mock("GET", "/key")
        .with_status(401)
        .with_header("content-type", "application/json")
        .with_header("retry-after", "7")
        .with_body("x".repeat(70 * 1024))
        .expect(1)
        .create_async()
        .await;

    let error = ShimClient::new()
        .credential_check(&provider(server.url()))
        .await
        .expect_err("the body is oversized");
    match error {
        ShimError::ProviderError {
            status,
            body,
            retry_after,
        } => {
            assert_eq!(status, 401);
            assert_eq!(retry_after, Some(std::time::Duration::from_secs(7)));
            assert_eq!(body, "the provider rejected the credential");
        }
        other => panic!("expected a provider error, got {other:?}"),
    }
}

#[tokio::test]
async fn an_error_inside_a_success_response_uses_the_same_safe_contract() {
    let mut server = mockito::Server::new_async().await;
    server
        .mock("GET", "/key")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(format!(
            r#"{{"error":{{"code":401,"message":"refused {KEY}"}}}}"#
        ))
        .expect(1)
        .create_async()
        .await;

    let error = ShimClient::new()
        .credential_check(&provider(server.url()))
        .await
        .expect_err("the body rejects the key");
    match error {
        ShimError::ProviderError { status, body, .. } => {
            assert_eq!(status, 401);
            assert_eq!(body, "the provider rejected the credential");
        }
        other => panic!("expected a provider error, got {other:?}"),
    }
}
