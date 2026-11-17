//! `llmshim-control`: the control plane's server (design notes 0024, 0026).
//!
//! ```text
//! llmshim-control serve (--db <FILE> | --database-url-file <FILE> [--database-schema <NAME>])
//!                       --listen <ADDR> --token-file <FILE> [--signing-key-file <FILE>]
//! ```
//!
//! Exit status: 0 clean shutdown, 1 it could not start (a file, the address, the database),
//! 2 bad arguments.

use llmshim_control::api::{router, seed_file_problem, token_problem, AppState};
use llmshim_control::Store;
use std::path::PathBuf;

const USAGE: &str = "Usage: llmshim-control serve (--db <FILE> | --database-url-file <FILE> [--database-schema <NAME>])\n                              --listen <ADDR> --token-file <FILE> [--signing-key-file <FILE>]\n\n  --db                SQLite database (created owner-only if absent)\n  --database-url-file a file holding a postgres:// URL (owner-only on Unix: it holds the\n                      password); TLS with the web PKI roots unless the URL says\n                      sslmode=disable. Exactly one of --db and this\n  --database-schema   with Postgres, the schema to use (created if absent)\n  --listen            address to listen on, for example 127.0.0.1:3100\n  --token-file        the API's bearer token (at least 22 visible ASCII characters;\n                      on Unix the file must not be readable by group or others)\n  --signing-key-file  a signing seed (`llmshim snapshot keygen` writes one); read at each\n                      publish, never at start. Without it publishing is unavailable.";

struct Options {
    db: Option<PathBuf>,
    database_url_file: Option<PathBuf>,
    database_schema: Option<String>,
    listen: String,
    token_file: PathBuf,
    signing_key_file: Option<PathBuf>,
}

fn parse(args: &[String]) -> Result<Options, String> {
    if args.first().map(String::as_str) != Some("serve") {
        return Err("the only command is serve".into());
    }
    let (mut db, mut listen, mut token, mut signing) = (None, None, None, None);
    let (mut url_file, mut schema) = (None, None);
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
            "--database-url-file" => url_file = Some(PathBuf::from(value)),
            "--database-schema" => schema = Some(value),
            "--listen" => listen = Some(value),
            "--token-file" => token = Some(PathBuf::from(value)),
            "--signing-key-file" => signing = Some(PathBuf::from(value)),
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    if db.is_some() == url_file.is_some() {
        return Err("give exactly one of --db and --database-url-file".into());
    }
    if schema.is_some() && url_file.is_none() {
        return Err("--database-schema is for --database-url-file".into());
    }
    Ok(Options {
        db,
        database_url_file: url_file,
        database_schema: schema,
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

/// The store the options name. A Postgres URL holds the password, so its file is owner-only and
/// no error ever carries it; a SQLite file is created owner-only from its first byte.
fn open_store(options: &Options) -> Result<Store, String> {
    if let Some(url_file) = &options.database_url_file {
        let url = read_owner_only(url_file)?.trim().to_string();
        // Connect and initialize on a thread outside the async runtime (see `Store`).
        return Store::open_postgres(&url, options.database_schema.as_deref())
            .map_err(|error| format!("cannot open the database: {error}"));
    }
    let db = options.db.as_ref().expect("one of the two was checked");
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        if !db.exists() {
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(db)
                .map_err(|error| {
                    format!(
                        "cannot create the database {}: {}",
                        db.display(),
                        error.kind()
                    )
                })?;
        }
    }
    Store::open(db).map_err(|error| format!("cannot open the database {}: {error}", db.display()))
}

fn read_owner_only(path: &std::path::Path) -> Result<String, String> {
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
    std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {}", path.display(), e.kind()))
}

async fn run(options: Options) -> i32 {
    let token = match read_token(&options.token_file) {
        Ok(token) => token,
        Err(message) => {
            eprintln!("{message}");
            return 1;
        }
    };
    if let Some(problem) = options
        .signing_key_file
        .as_deref()
        .and_then(seed_file_problem)
    {
        eprintln!("{problem}");
        return 1;
    }
    let store = match open_store(&options) {
        Ok(store) => store,
        Err(message) => {
            eprintln!("{message}");
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
