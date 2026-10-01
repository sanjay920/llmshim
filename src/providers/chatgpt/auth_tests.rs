use super::super::tokens::now;
use super::*;

struct EnvironmentVariableRestore {
    name: &'static str,
    previous_value: Option<std::ffi::OsString>,
}

impl Drop for EnvironmentVariableRestore {
    fn drop(&mut self) {
        if let Some(previous_value) = &self.previous_value {
            std::env::set_var(self.name, previous_value);
        } else {
            std::env::remove_var(self.name);
        }
    }
}

#[tokio::test]
async fn pending_device_authorization_waits_and_times_out() {
    let mut server = mockito::Server::new_async().await;
    let pending = server
        .mock("POST", "/api/accounts/deviceauth/token")
        .with_status(403)
        .expect(1)
        .create_async()
        .await;
    let dir = tempfile::tempdir().unwrap();
    let auth = ChatGptAuth::new(dir.path().join("auth.json")).with_auth_base(server.url());
    let code = DeviceCode {
        verification_url: String::new(),
        user_code: "test".into(),
        device_auth_id: "device".into(),
        interval: Duration::from_secs(5),
        deadline: Instant::now() + Duration::from_secs(1),
    };
    let err = auth.finish_login(code).await.unwrap_err();
    assert!(matches!(err, ShimError::ProviderError { status: 408, .. }));
    pending.assert_async().await;
    assert!(!auth.auth_path().exists());
}

#[tokio::test]
async fn expired_device_code_does_not_start_polling() {
    let dir = tempfile::tempdir().unwrap();
    let auth = ChatGptAuth::new(dir.path().join("auth.json"));
    let code = DeviceCode {
        verification_url: String::new(),
        user_code: "test".into(),
        device_auth_id: "device".into(),
        interval: Duration::from_secs(5),
        deadline: Instant::now() - Duration::from_secs(1),
    };
    let err = auth.finish_login(code).await.unwrap_err();
    assert!(matches!(err, ShimError::ProviderError { status: 408, .. }));
}

#[test]
fn unknown_or_expired_token_expiry_is_rejected() {
    for value in [
        json!({"access_token": "private"}),
        json!({"access_token": "private", "expires_at": 1}),
        json!({"access_token": "", "expires_in": 3600}),
        json!({"refresh_token": "private"}),
    ] {
        let err = Tokens::from_response(value, None).err().unwrap();
        assert!(!err.to_string().contains("private"));
    }
}

#[cfg(unix)]
#[test]
fn protected_default_auth_read_keeps_its_construction_home() {
    let previous_home = EnvironmentVariableRestore {
        name: "HOME",
        previous_value: std::env::var_os("HOME"),
    };
    let previous_token_directory = EnvironmentVariableRestore {
        name: "CHATGPT_TOKEN_DIR",
        previous_value: std::env::var_os("CHATGPT_TOKEN_DIR"),
    };
    let previous_auth_file = EnvironmentVariableRestore {
        name: "CHATGPT_AUTH_FILE",
        previous_value: std::env::var_os("CHATGPT_AUTH_FILE"),
    };
    let construction_home_directory = tempfile::tempdir().unwrap();
    let changed_home_directory = tempfile::tempdir().unwrap();
    let construction_auth_directory = construction_home_directory.path().join(".llmshim/chatgpt");
    std::fs::create_dir_all(&construction_auth_directory).unwrap();
    std::fs::write(
        construction_auth_directory.join("auth.json"),
        format!(
            r#"{{"access_token":"synthetic","expires_at":{}}}"#,
            now().saturating_add(3600)
        ),
    )
    .unwrap();
    std::env::set_var("HOME", construction_home_directory.path());
    std::env::remove_var("CHATGPT_TOKEN_DIR");
    std::env::remove_var("CHATGPT_AUTH_FILE");
    let auth = ChatGptAuth::from_env();

    std::env::set_var("HOME", changed_home_directory.path());
    assert_eq!(auth.status().unwrap(), LoginStatus::Ready);
    assert_eq!(
        auth.auth_path(),
        construction_auth_directory.join("auth.json")
    );

    drop(previous_auth_file);
    drop(previous_token_directory);
    drop(previous_home);
}
