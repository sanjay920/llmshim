//! `llmshim-control`: the control plane's server (design notes 0024, 0026).
//!
//! ```text
//! llmshim-control serve --db <FILE> --listen <ADDR> --token-file <FILE> [--signing-key-file <FILE>]
//! ```
//!
//! Exit status: 0 clean shutdown, 1 it could not start (a file, the address, the database),
//! 2 bad arguments.

use llmshim_control::api::{router, token_problem, AppState};
use llmshim_control::Store;
use std::path::PathBuf;

const USAGE: &str = "Usage: llmshim-control serve --db <FILE> --listen <ADDR> --token-file <FILE> [--signing-key-file <FILE>]\n\n  --db                SQLite database (created if absent)\n  --listen            address to listen on, for example 127.0.0.1:3100\n  --token-file        the API's bearer token (at least 22 visible ASCII characters;\n                      on Unix the file must not be readable by group or others)\n  --signing-key-file  a signing seed (`llmshim snapshot keygen` writes one); read at each\n                      publish, never at start. Without it publishing is unavailable.";

struct Options {
    db: PathBuf,
    listen: String,
    token_file: PathBuf,
    signing_key_file: Option<PathBuf>,
}

fn parse(args: &[String]) -> Result<Options, String> {
    if args.first().map(String::as_str) != Some("serve") {
        return Err("the only command is serve".into());
    }
    let (mut db, mut listen, mut token, mut signing) = (None, None, None, None);
    let mut at = 1;
    while at < args.len() {
        let flag = args[at].as_str();
        at += 1;
        let value = args
            .get(at)
            .filter(|v| !v.starts_with("--"))
            .ok_or_else(|| format!("{flag} requires a value"))?
            .clone();
        at += 1;
        match flag {
            "--db" => db = Some(PathBuf::from(value)),
            "--listen" => listen = Some(value),
            "--token-file" => token = Some(PathBuf::from(value)),
            "--signing-key-file" => signing = Some(PathBuf::from(value)),
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(Options {
        db: db.ok_or("--db is required")?,
        listen: listen.ok_or("--listen is required")?,
        token_file: token.ok_or("--token-file is required")?,
        signing_key_file: signing,
    })
}

fn read_token(path: &std::path::Path) -> Result<String, String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)
            .map_err(|e| format!("cannot read {}: {}", path.display(), e.kind()))?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            return Err(format!(
                "{} is readable by group or others; make it owner-only (chmod 600)",
                path.display()
            ));
        }
    }
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {}", path.display(), e.kind()))?;
    let token = text.trim().to_string();
    match token_problem(&token) {
        None => Ok(token),
        Some(problem) => Err(format!("the token in {} {problem}", path.display())),
    }
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{USAGE}");
        return;
    }
    let options = match parse(&args) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("{message}\n\n{USAGE}");
            std::process::exit(2);
        }
    };
    std::process::exit(run(options).await);
}

async fn run(options: Options) -> i32 {
    let token = match read_token(&options.token_file) {
        Ok(token) => token,
        Err(message) => {
            eprintln!("{message}");
            return 1;
        }
    };
    let store = match Store::open(&options.db) {
        Ok(store) => store,
        Err(error) => {
            eprintln!("cannot open the database {}: {error}", options.db.display());
            return 1;
        }
    };
    let listener = match tokio::net::TcpListener::bind(&options.listen).await {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("cannot listen on {}: {}", options.listen, error.kind());
            return 1;
        }
    };
    let address = match listener.local_addr() {
        Ok(address) => address,
        Err(error) => {
            eprintln!("cannot read the listener address: {}", error.kind());
            return 1;
        }
    };
    if !address.ip().is_loopback() {
        eprintln!(
            "warning: listening on {address} without TLS: put a TLS-terminating proxy in front of it"
        );
    }
    let state = AppState::new(store, &token, options.signing_key_file);
    eprintln!("llmshim-control listening on http://{address}");
    let shutdown = async {
        #[cfg(unix)]
        {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("a SIGTERM handler");
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
    };
    match axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown)
        .await
    {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("the server stopped: {error}");
            1
        }
    }
}
