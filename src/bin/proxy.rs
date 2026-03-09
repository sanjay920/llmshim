use std::net::SocketAddr;

#[tokio::main]
async fn main() {
    // Load config: env vars > .env > ~/.llmshim/config.toml
    llmshim::env::load_all();

    let router = llmshim::router::Router::from_env();
    let providers = router.provider_keys();

    if providers.is_empty() {
        eprintln!("No API keys found. Configure with:");
        eprintln!("  llmshim-config configure");
        eprintln!(
            "  # or set env vars: OPENAI_API_KEY, ANTHROPIC_API_KEY, GEMINI_API_KEY, XAI_API_KEY"
        );
        std::process::exit(1);
    }

    // Optional file logging
    let logger = std::env::var("LLMSHIM_LOG")
        .ok()
        .and_then(|path| llmshim::log::Logger::to_file(&path).ok());

    // Read proxy config from config file, then allow env var overrides
    let config = llmshim::config::load();
    let host = std::env::var("LLMSHIM_HOST").unwrap_or(config.proxy.host);
    let port: u16 = std::env::var("LLMSHIM_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(config.proxy.port);
    let addr: SocketAddr = format!("{}:{}", host, port)
        .parse()
        .expect("Invalid address");

    eprintln!("llmshim proxy starting on http://{}", addr);
    eprintln!("  Providers: {:?}", providers);
    eprintln!("  Endpoints:");
    eprintln!("    POST /v1/chat          — completion");
    eprintln!("    POST /v1/chat/stream   — streaming SSE");
    eprintln!("    GET  /v1/models        — list models");
    eprintln!("    GET  /health           — health check");

    let app = llmshim::proxy::app(router, logger);
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
