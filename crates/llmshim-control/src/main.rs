//! `llmshim-control`: the control plane's server (design notes 0024, 0026).
//!
//! ```text
//! llmshim-control serve (--db <FILE> | --database-url-file <FILE> [--database-schema <NAME>])
//!                       --listen <ADDR> --token-file <FILE> [--signing-key-file <FILE>]
//! ```
//!
//! Exit status: 0 clean shutdown, 1 it could not start (a file, the address, the database),
//! 2 bad arguments.

use llmshim_control::api::{router, seed_file_problem, AppState};
use llmshim_control::Store;
use std::path::PathBuf;

const USAGE: &str = "Usage: llmshim-control audit-verify (--db <FILE> | --database-url-file <FILE> [--database-schema <NAME>] [--database-ca-file <FILE>])\n         checks the change history's hash chain; exit 1 names the first broken row\n\n       llmshim-control serve (--db <FILE> | --database-url-file <FILE> [--database-schema <NAME>])\n                              --listen <ADDR> --token-file <FILE> [--signing-key-file <FILE>]\n\n  --db                SQLite database (created owner-only if absent)\n  --database-url-file a file holding a postgres:// URL (owner-only on Unix: it holds the\n                      password). Exactly one of --db and this\n  --database-schema   with Postgres, the schema to use (created if absent)\n  --database-ca-file  with Postgres, PEM certificates to trust instead of the web PKI roots\n                      (a private or provider CA). TLS is required unless the URL says\n                      sslmode=disable\n  --listen            address to listen on, for example 127.0.0.1:3100\n  --token-file        the API's bearer token (at least 22 visible ASCII characters;\n                      on Unix the file must not be readable by group or others)\n  --sso-config        a JSON issuer file (note 0049): verify OIDC access tokens, take the actor from\n                      the verified subject and the role from its `roles` claim (control:read,\n                      control:key-issuer, control:admin). Then --token-file is the break-glass\n                      token, required unless the file sets \"break_glass\": false\n  --ingest-token-file a token (owner-only, a different one from --token-file) that the gateways' usage\n                      push (`[usage_export]`) presents to POST /v1/usage/batch: accepted there and nowhere\n                      else. Without it the push is refused (note 0051)\n  --gateways-file     a JSON file listing the gateways' admin listeners (name, admin_url, token_file, ca_file)
                      so the UI can show their providers, circuits, routes and configuration state (note 0051).
                      Read-only; without it that view says none are configured
  --allow-payload-views-without-sso\n                      let a caller whose identity is not verified (one shared token, break-glass) open captured\n                      payloads. Without it only a person signed in through the identity provider can: the history\n                      records who opened a payload, and with a shared token the name is a label the caller typed\n  --request-retention-days N  keep the request ledger N days (1 to 3650, default 30)\n  --signing-key-file  a signing seed (`llmshim snapshot keygen` writes one); read at each\n                      publish, never at start. Without it publishing is unavailable.";

struct Options {
    db: Option<PathBuf>,
    database_url_file: Option<PathBuf>,
    database_schema: Option<String>,
    database_ca_file: Option<PathBuf>,
    verify: bool,
    listen: String,
    token_file: Option<PathBuf>,
    sso_config: Option<PathBuf>,
    signing_key_file: Option<PathBuf>,
    ingest_token_file: Option<PathBuf>,
    gateways_file: Option<PathBuf>,
    allow_payload_views_without_sso: bool,
    request_retention_days: u32,
}

fn parse(args: &[String]) -> Result<Options, String> {
    if !matches!(
        args.first().map(String::as_str),
        Some("serve" | "audit-verify")
    ) {
        return Err("the commands are serve and audit-verify".into());
    }
    let verify = args[0] == "audit-verify";
    let (mut db, mut listen, mut token, mut signing) = (None, None, None, None);
    let (mut url_file, mut schema, mut ca) = (None, None, None);
    let mut sso = None;
    let (mut ingest, mut retention, mut gateways) = (None, None, None);
    let mut allow_payloads = false;
    let mut at = 1;
    while at < args.len() {
        let flag = args[at].as_str();
        at += 1;
        // The one flag that takes no value.
        if flag == "--allow-payload-views-without-sso" {
            allow_payloads = true;
            continue;
        }
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
            "--database-ca-file" => ca = Some(PathBuf::from(value)),
            "--listen" => listen = Some(value),
            "--token-file" => token = Some(PathBuf::from(value)),
            "--signing-key-file" => signing = Some(PathBuf::from(value)),
            "--sso-config" => sso = Some(PathBuf::from(value)),
            "--ingest-token-file" => ingest = Some(PathBuf::from(value)),
            "--gateways-file" => gateways = Some(PathBuf::from(value)),
            "--request-retention-days" => {
                retention = Some(
                    value
                        .parse::<u32>()
                        .ok()
                        .filter(|days| (1..=3650).contains(days))
                        .ok_or("--request-retention-days must be 1 to 3650")?,
                )
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    if db.is_some() == url_file.is_some() {
        return Err("give exactly one of --db and --database-url-file".into());
    }
    if (schema.is_some() || ca.is_some()) && url_file.is_none() {
        return Err("--database-schema and --database-ca-file are for --database-url-file".into());
    }
    if verify
        && (listen.is_some()
            || token.is_some()
            || signing.is_some()
            || sso.is_some()
            || ingest.is_some()
            || gateways.is_some()
            || allow_payloads
            || retention.is_some())
    {
        return Err("audit-verify takes only the database options".into());
    }
    Ok(Options {
        db,
        database_url_file: url_file,
        database_schema: schema,
        database_ca_file: ca,
        verify,
        listen: if verify {
            String::new()
        } else {
            listen.ok_or("--listen is required")?
        },
        // Without an identity provider the one token is the only credential. With one it is the
        // break-glass token, and whether it is needed depends on the issuer file (read in `run`).
        token_file: if verify || sso.is_some() {
            token
        } else {
            Some(token.ok_or("--token-file is required")?)
        },
        sso_config: sso,
        signing_key_file: signing,
        ingest_token_file: ingest,
        gateways_file: gateways,
        allow_payload_views_without_sso: allow_payloads,
        request_retention_days: retention.unwrap_or(30),
    })
}

fn read_token(path: &std::path::Path) -> Result<String, String> {
    llmshim_control::gateways::read_token_file(path)
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
    if options.verify {
        std::process::exit(audit_verify(&options));
    }
    std::process::exit(run(options).await);
}

/// `audit-verify`: read-only on the history (the store's own migration still runs, as for any open),
/// and never creates a SQLite file (a Postgres target is opened like any other, so a mistyped
/// schema is initialized empty and reports zero rows).
fn audit_verify(options: &Options) -> i32 {
    if let Some(db) = &options.db {
        if std::fs::metadata(db).map_or(true, |m| m.len() == 0) {
            eprintln!("{} does not exist or is empty", db.display());
            return 1;
        }
    }
    let mut store = match open_store(options) {
        Ok(store) => store,
        Err(message) => {
            eprintln!("{message}");
            return 1;
        }
    };
    match store.verify_history() {
        Ok(Ok(done)) => {
            println!(
                "history: intact, {} chained row(s), {} before the chain, head {}",
                done.records, done.unchained, done.head
            );
            0
        }
        Ok(Err(broken)) => {
            println!("history: broken at row {}: {}", broken.id, broken.reason);
            1
        }
        Err(error) => {
            eprintln!("cannot read the history: {error}");
            1
        }
    }
}

/// The store the options name. A Postgres URL holds the password, so its file is owner-only and
/// no error ever carries it; a SQLite file is created owner-only from its first byte.
fn open_store(options: &Options) -> Result<Store, String> {
    if let Some(url_file) = &options.database_url_file {
        let url = read_owner_only(url_file)?.trim().to_string();
        // Connect and initialize on a thread outside the async runtime (see `Store`).
        return Store::open_postgres_with(
            &url,
            options.database_schema.as_deref(),
            options.database_ca_file.as_deref(),
        )
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

/// The issuer file, as an identity provider (note 0049). Every problem names the file and the
/// field, never a value.
fn read_sso(path: &std::path::Path) -> Result<llmshim_control::sso::Sso, String> {
    let bytes =
        std::fs::read(path).map_err(|e| format!("cannot read {}: {}", path.display(), e.kind()))?;
    if bytes.len() > 64 * 1024 {
        return Err(format!("{} is larger than 64 KiB", path.display()));
    }
    let config: llmshim_control::sso::SsoConfig = serde_json::from_slice(&bytes).map_err(|e| {
        format!(
            "{} is not a valid issuer file: {}",
            path.display(),
            e.classify_name()
        )
    })?;
    llmshim_control::sso::Sso::from_config(&config)
        .map_err(|message| format!("{}: {message}", path.display()))
}

trait ClassifyName {
    fn classify_name(&self) -> &'static str;
}

impl ClassifyName for serde_json::Error {
    /// The kind of parse failure, not the error's text (which can quote a value).
    fn classify_name(&self) -> &'static str {
        match self.classify() {
            serde_json::error::Category::Io => "unreadable",
            serde_json::error::Category::Syntax | serde_json::error::Category::Eof => {
                "not valid JSON"
            }
            serde_json::error::Category::Data => "a field is missing, unknown or of the wrong type",
        }
    }
}

async fn run(options: Options) -> i32 {
    let sso = match options.sso_config.as_deref().map(read_sso).transpose() {
        Ok(sso) => sso,
        Err(message) => {
            eprintln!("{message}");
            return 1;
        }
    };
    if let Some(sso) = &sso {
        // The token is either needed or refused, never silently ignored.
        match (sso.break_glass(), &options.token_file) {
            (true, None) => {
                eprintln!("--token-file is required while the issuer file leaves break_glass on");
                return 2;
            }
            (false, Some(_)) => {
                eprintln!("--token-file is given but the issuer file sets break_glass to false");
                return 2;
            }
            _ => {}
        }
    }
    let token = match options.token_file.as_deref().map(read_token).transpose() {
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
    let shared_token = token.clone();
    let state = match (sso, token) {
        (Some(sso), token) => {
            AppState::with_sso(store, token.as_deref(), options.signing_key_file, sso)
        }
        (None, Some(token)) => AppState::new(store, &token, options.signing_key_file),
        (None, None) => unreachable!("parse requires a token without an issuer file"),
    };
    if options.allow_payload_views_without_sso {
        eprintln!("warning: payloads may be opened by a caller whose identity is not verified; the history names the label they gave");
        state.allow_unverified_payload_views(true);
    }
    if let Some(file) = &options.gateways_file {
        let bytes = match std::fs::read(file) {
            Ok(bytes) => bytes,
            Err(error) => {
                eprintln!("cannot read {}: {}", file.display(), error.kind());
                return 1;
            }
        };
        match llmshim_control::gateways::Gateways::from_bytes(&bytes) {
            Ok(gateways) => state.set_gateways(gateways),
            Err(message) => {
                eprintln!("{}: {message}", file.display());
                return 1;
            }
        }
    }
    if let Some(file) = &options.ingest_token_file {
        let ingest = match read_token(file) {
            Ok(token) => token,
            Err(message) => {
                eprintln!("{message}");
                return 1;
            }
        };
        // A credential for the usage push only: it must not also be the one that administers.
        if shared_token.as_deref() == Some(ingest.as_str()) {
            eprintln!("--ingest-token-file must hold a different token from --token-file");
            return 2;
        }
        state.set_ingest_token(&ingest);
    }
    {
        // The request ledger keeps `--request-retention-days` (30 by default) and deletes older rows
        // itself, hourly.
        let state = state.clone();
        let days = i64::from(options.request_retention_days);
        tokio::spawn(async move {
            loop {
                let before = chrono::Utc::now().timestamp_millis() - days * 86_400_000;
                let state = state.clone();
                match tokio::task::spawn_blocking(move || state.purge_requests(before)).await {
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => eprintln!("request ledger retention purge failed: {error}"),
                    Err(error) => eprintln!("request ledger retention purge failed: {error}"),
                }
                tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
            }
        });
    }
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
