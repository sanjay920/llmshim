#![cfg(unix)]
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const TOKEN: &str = "control-token-0123456789abcdef";

fn dir(label: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "llmshim-control-bin-{label}-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn token_file(directory: &Path, text: &str, mode: u32) -> PathBuf {
    let path = directory.join("token");
    std::fs::write(&path, text).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
    path
}

fn run(args: &[&str]) -> (Option<i32>, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_llmshim-control"))
        .args(args)
        .env_clear()
        .output()
        .unwrap();
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn bad_arguments_and_bad_files_fail_before_serving() {
    let directory = dir("args");
    let db = directory.join("c.db");
    let db = db.to_str().unwrap();
    assert_eq!(run(&["serve"]).0, Some(2));
    assert_eq!(
        run(&["serve", "--db", db, "--listen", "127.0.0.1:0"]).0,
        Some(2),
        "a token file is required"
    );
    assert_eq!(run(&["serve", "--bogus", "x"]).0, Some(2));
    assert_eq!(run(&["frobnicate"]).0, Some(2));
    // A token anyone on the machine can read is refused.
    let open = token_file(&directory, TOKEN, 0o644);
    let (code, stderr) = run(&[
        "serve",
        "--db",
        db,
        "--listen",
        "127.0.0.1:0",
        "--token-file",
        open.to_str().unwrap(),
    ]);
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("owner-only"), "{stderr}");
    assert!(!stderr.contains(TOKEN));
    // A short token is refused without echoing it.
    let short = token_file(&directory, "short-token", 0o600);
    let (code, stderr) = run(&[
        "serve",
        "--db",
        db,
        "--listen",
        "127.0.0.1:0",
        "--token-file",
        short.to_str().unwrap(),
    ]);
    assert_eq!(code, Some(1), "{stderr}");
    assert!(
        stderr.contains("at least 22") && !stderr.contains("short-token"),
        "{stderr}"
    );
    // A missing token file.
    let (code, _) = run(&[
        "serve",
        "--db",
        db,
        "--listen",
        "127.0.0.1:0",
        "--token-file",
        "/nonexistent/token",
    ]);
    assert_eq!(code, Some(1));
}

fn http(
    address: &str,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&str>,
) -> (u16, String) {
    let mut stream = TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut request =
        format!("{method} {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n");
    if let Some(token) = token {
        request += &format!("Authorization: Bearer {token}\r\n");
    }
    if let Some(body) = body {
        request += &format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        );
    }
    request += "\r\n";
    stream.write_all(request.as_bytes()).unwrap();
    if let Some(body) = body {
        stream.write_all(body.as_bytes()).unwrap();
    }
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let status = response.split_whitespace().nth(1).unwrap().parse().unwrap();
    let body = response.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    (status, body)
}

#[test]
fn the_server_serves_over_a_socket_authenticates_and_stops_on_sigterm() {
    let directory = dir("serve");
    let token = token_file(&directory, &format!("{TOKEN}\n"), 0o600);
    let db = directory.join("c.db");
    let mut child = Command::new(env!("CARGO_BIN_EXE_llmshim-control"))
        .args([
            "serve",
            "--db",
            db.to_str().unwrap(),
            "--listen",
            "127.0.0.1:0",
            "--token-file",
            token.to_str().unwrap(),
        ])
        .env_clear()
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
    let deadline = Instant::now() + Duration::from_secs(20);
    let address = loop {
        assert!(Instant::now() < deadline, "the server did not start");
        let line = lines.next().expect("stderr closed").unwrap();
        if let Some(rest) = line.split("listening on http://").nth(1) {
            break rest.trim().to_string();
        }
    };
    assert_eq!(http(&address, "GET", "/v1/orgs", None, None).0, 401);
    assert_eq!(
        http(
            &address,
            "GET",
            "/v1/orgs",
            Some("wrong-token-0123456789abcdef"),
            None
        )
        .0,
        401
    );
    let (status, body) = http(&address, "GET", "/v1/orgs", Some(TOKEN), None);
    assert_eq!((status, body.trim()), (200, r#"{"orgs":[]}"#));
    let (status, body) = http(
        &address,
        "POST",
        "/v1/orgs",
        Some(TOKEN),
        Some(r#"{"name":"Acme"}"#),
    );
    assert_eq!(status, 201, "{body}");
    // The data is in the file: a second process would see it.
    assert!(db.exists());
    let killed = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    let deadline = Instant::now() + Duration::from_secs(20);
    let code = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status.code();
        }
        assert!(
            Instant::now() < deadline,
            "the server did not stop on SIGTERM"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(code, Some(0));
}
