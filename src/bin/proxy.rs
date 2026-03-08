use std::net::SocketAddr;

fn load_env() {
    if let Ok(contents) = std::fs::read_to_string(".env") {
        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((key, value)) = line.split_once('=') {
                let value = value.trim();
                let value = value
                    .strip_prefix('"')
                    .and_then(|v| v.strip_suffix('"'))
                    .unwrap_or(value);
                std::env::set_var(key.trim(), value);
            }
        }
    }
}

#[tokio::main]
async fn main() {
    load_env();

    let router = llmshim::router::Router::from_env();
    let providers = router.provider_keys();

    if providers.is_empty() {
        eprintln!("No API keys found. Set OPENAI_API_KEY, ANTHROPIC_API_KEY, GEMINI_API_KEY, or XAI_API_KEY.");
        std::process::exit(1);
    }

    // Optional file logging
    let logger = std::env::var("LLMSHIM_LOG")
        .ok()
        .and_then(|path| llmshim::log::Logger::to_file(&path).ok());

    let host = std::env::var("LLMSHIM_HOST").unwrap_or_else(|_| "0.0.0.0".to_string());
    let port: u16 = std::env::var("LLMSHIM_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3000);
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
