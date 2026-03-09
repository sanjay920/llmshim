use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::terminal;
use futures::StreamExt;
use llmshim::log::{LogEntry, Logger, RequestTimer};
use serde_json::{json, Value};
use std::io::{self, Write};

const MODELS: &[(&str, &str)] = &[
    ("openai/gpt-5.4", "GPT-5.4"),
    ("anthropic/claude-opus-4-6", "Claude Opus 4.6"),
    ("anthropic/claude-sonnet-4-6", "Claude Sonnet 4.6"),
    ("anthropic/claude-haiku-4-5-20251001", "Claude Haiku 4.5"),
    ("gemini/gemini-3.1-pro-preview", "Gemini 3.1 Pro"),
    ("gemini/gemini-3-flash-preview", "Gemini 3 Flash"),
    (
        "gemini/gemini-3.1-flash-lite-preview",
        "Gemini 3.1 Flash Lite",
    ),
    ("xai/grok-4-1-fast-reasoning", "Grok 4.1 Fast Reasoning"),
    ("xai/grok-4-1-fast-non-reasoning", "Grok 4.1 Fast"),
];

fn print_models(current: &str) {
    println!("\n  Available models:");
    for (i, (id, label)) in MODELS.iter().enumerate() {
        let marker = if *id == current { " ←" } else { "" };
        println!("    {}. {} ({}){}", i + 1, label, id, marker);
    }
    println!();
}

fn print_help() {
    println!("\n  Commands:");
    println!("    /model         - switch model");
    println!("    /models        - list available models");
    println!("    /image <path>  - attach an image file");
    println!("    /paste         - attach image from clipboard");
    println!("    /clear         - clear conversation history");
    println!("    /history       - show message count");
    println!("    /quit          - exit");
    println!();
    println!("  Images:");
    println!("    You can also include image paths inline in your message:");
    println!("    > Describe this image /path/to/photo.jpg");
    println!();
}

/// Detect image file extensions.
fn is_image_path(path: &str) -> bool {
    let lower = path.to_lowercase();
    lower.ends_with(".jpg")
        || lower.ends_with(".jpeg")
        || lower.ends_with(".png")
        || lower.ends_with(".gif")
        || lower.ends_with(".webp")
        || lower.ends_with(".bmp")
        || lower.ends_with(".svg")
}

/// Infer MIME type from file extension.
fn mime_from_path(path: &str) -> &'static str {
    let lower = path.to_lowercase();
    if lower.ends_with(".png") {
        "image/png"
    } else if lower.ends_with(".gif") {
        "image/gif"
    } else if lower.ends_with(".webp") {
        "image/webp"
    } else if lower.ends_with(".svg") {
        "image/svg+xml"
    } else if lower.ends_with(".bmp") {
        "image/bmp"
    } else {
        "image/jpeg"
    }
}

/// Read a file and return a base64 image content block, or None on failure.
fn image_file_to_block(path: &str) -> Option<Value> {
    let path = path.trim().trim_matches('"').trim_matches('\'');
    let expanded = if path.starts_with('~') {
        if let Ok(home) = std::env::var("HOME") {
            path.replacen('~', &home, 1)
        } else {
            path.to_string()
        }
    } else {
        path.to_string()
    };

    let data = std::fs::read(&expanded).ok()?;
    let b64 = base64_encode(&data);
    let mime = mime_from_path(&expanded);
    Some(json!({
        "type": "image_url",
        "image_url": {"url": format!("data:{};base64,{}", mime, b64)}
    }))
}

/// Simple base64 encoder (no external dependency needed).
fn base64_encode(data: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let triple = (b0 << 16) | (b1 << 8) | b2;
        result.push(CHARS[((triple >> 18) & 0x3F) as usize] as char);
        result.push(CHARS[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            result.push(CHARS[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }
        if chunk.len() > 2 {
            result.push(CHARS[(triple & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }
    }
    result
}

/// Try to get an image from the system clipboard (macOS: pbpaste, Linux: xclip).
fn clipboard_image() -> Option<Value> {
    // macOS: check if clipboard has image data
    #[cfg(target_os = "macos")]
    {
        // Check clipboard type
        let output = std::process::Command::new("osascript")
            .arg("-e")
            .arg("clipboard info")
            .output()
            .ok()?;
        let info = String::from_utf8_lossy(&output.stdout);
        if !info.contains("TIFF") && !info.contains("PNGf") && !info.contains("JPEG") {
            return None;
        }

        // Get clipboard image as PNG using osascript
        let output = std::process::Command::new("osascript")
            .arg("-e")
            .arg(
                r#"set theFile to (POSIX path of (path to temporary items)) & "llmshim_clipboard.png"
set theImage to the clipboard as «class PNGf»
set fp to open for access theFile with write permission
write theImage to fp
close access fp
return theFile"#,
            )
            .output()
            .ok()?;

        if !output.status.success() {
            return None;
        }

        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let data = std::fs::read(&path).ok()?;
        let _ = std::fs::remove_file(&path); // cleanup
        let b64 = base64_encode(&data);
        Some(json!({
            "type": "image_url",
            "image_url": {"url": format!("data:image/png;base64,{}", b64)}
        }))
    }

    // Linux: try xclip
    #[cfg(target_os = "linux")]
    {
        let output = std::process::Command::new("xclip")
            .args(["-selection", "clipboard", "-t", "image/png", "-o"])
            .output()
            .ok()?;
        if !output.status.success() || output.stdout.is_empty() {
            return None;
        }
        let b64 = base64_encode(&output.stdout);
        return Some(json!({
            "type": "image_url",
            "image_url": {"url": format!("data:image/png;base64,{}", b64)}
        }));
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    None
}

/// Parse user input to extract text and any inline image file paths.
/// Returns content as either a string (no images) or an array of content blocks.
fn parse_input_with_images(input: &str, pending_images: &mut Vec<Value>) -> Value {
    // Check for inline image paths
    let words: Vec<&str> = input.split_whitespace().collect();
    let mut text_parts: Vec<String> = Vec::new();
    let mut _images_found = false;

    for word in &words {
        let clean = word.trim_matches('"').trim_matches('\'');
        if is_image_path(clean) && std::path::Path::new(clean).exists() {
            if let Some(block) = image_file_to_block(clean) {
                pending_images.push(block);
                _images_found = true;
                continue;
            }
        }
        // Also check with ~ expansion
        if is_image_path(clean) {
            let expanded = if clean.starts_with('~') {
                std::env::var("HOME")
                    .map(|h| clean.replacen('~', &h, 1))
                    .unwrap_or_else(|_| clean.to_string())
            } else {
                clean.to_string()
            };
            if std::path::Path::new(&expanded).exists() {
                if let Some(block) = image_file_to_block(clean) {
                    pending_images.push(block);
                    _images_found = true;
                    continue;
                }
            }
        }
        text_parts.push(word.to_string());
    }

    let text = text_parts.join(" ");

    if pending_images.is_empty() {
        // No images — plain string
        json!(text)
    } else {
        // Build content blocks array
        let mut blocks: Vec<Value> = Vec::new();
        if !text.is_empty() {
            blocks.push(json!({"type": "text", "text": text}));
        }
        blocks.append(pending_images);
        json!(blocks)
    }
}

/// Result from the raw input reader.
struct RawInput {
    /// The plain text the user typed (for command detection).
    text: String,
    /// Ordered content blocks preserving interleaved text + images.
    /// Empty if no images were pasted (plain text message).
    blocks: Vec<Value>,
    /// Whether any images were included.
    has_images: bool,
}

/// Read a line of input using raw terminal mode.
/// Intercepts Ctrl-V to check clipboard for images.
/// Tracks interleaving: text typed before/between/after images is preserved in order.
/// Returns None on EOF/Ctrl-C/Ctrl-D.
fn read_line_raw() -> Option<RawInput> {
    let mut current_text = String::new();
    let mut blocks: Vec<Value> = Vec::new();
    let mut has_images = false;
    let mut image_count = 0;

    terminal::enable_raw_mode().ok()?;

    let result = loop {
        match event::read() {
            Ok(Event::Key(KeyEvent {
                code, modifiers, ..
            })) => {
                match (code, modifiers) {
                    // Ctrl-V: check clipboard for image
                    (KeyCode::Char('v'), m) if m.contains(KeyModifiers::CONTROL) => {
                        match clipboard_image() {
                            Some(block) => {
                                // Flush any accumulated text as a text block
                                let trimmed = current_text.trim().to_string();
                                if !trimmed.is_empty() {
                                    blocks.push(json!({"type": "text", "text": trimmed}));
                                }
                                current_text.clear();

                                blocks.push(block);
                                has_images = true;
                                image_count += 1;
                                print!("\x1b[2m[image {} pasted]\x1b[0m ", image_count);
                                io::stdout().flush().ok();
                            }
                            None => {
                                // No image — paste text from clipboard
                                if let Some(text) = clipboard_text() {
                                    print!("{}", text);
                                    io::stdout().flush().ok();
                                    current_text.push_str(&text);
                                }
                            }
                        }
                    }

                    // Ctrl-C or Ctrl-D: exit
                    (KeyCode::Char('c'), m) if m.contains(KeyModifiers::CONTROL) => {
                        println!();
                        break None;
                    }
                    (KeyCode::Char('d'), m) if m.contains(KeyModifiers::CONTROL) => {
                        println!();
                        break None;
                    }

                    // Enter: submit
                    (KeyCode::Enter, _) => {
                        println!();
                        // Flush remaining text
                        let trimmed = current_text.trim().to_string();
                        if !trimmed.is_empty() {
                            blocks.push(json!({"type": "text", "text": trimmed}));
                        }
                        let full_text = blocks
                            .iter()
                            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                            .collect::<Vec<_>>()
                            .join(" ");
                        break Some(RawInput {
                            text: full_text,
                            blocks,
                            has_images,
                        });
                    }

                    // Backspace: delete last char
                    (KeyCode::Backspace, _) => {
                        if !current_text.is_empty() {
                            current_text.pop();
                            print!("\x08 \x08");
                            io::stdout().flush().ok();
                        }
                    }

                    // Regular character
                    (KeyCode::Char(c), m) => {
                        if m.is_empty() || m == KeyModifiers::SHIFT {
                            current_text.push(c);
                            print!("{}", c);
                            io::stdout().flush().ok();
                        }
                    }

                    _ => {}
                }
            }
            Ok(Event::Paste(text)) => {
                print!("{}", text);
                io::stdout().flush().ok();
                current_text.push_str(&text);
            }
            Err(_) => break None,
            _ => {}
        }
    };

    terminal::disable_raw_mode().ok();
    result
}

/// Get text from clipboard (macOS).
fn clipboard_text() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("pbpaste").output().ok()?;
        if output.status.success() && !output.stdout.is_empty() {
            return String::from_utf8(output.stdout).ok();
        }
    }
    #[cfg(target_os = "linux")]
    {
        let output = std::process::Command::new("xclip")
            .args(["-selection", "clipboard", "-o"])
            .output()
            .ok()?;
        if output.status.success() && !output.stdout.is_empty() {
            return String::from_utf8(output.stdout).ok();
        }
    }
    None
}

fn model_label(id: &str) -> &str {
    MODELS
        .iter()
        .find(|(mid, _)| *mid == id)
        .map(|(_, label)| *label)
        .unwrap_or(id)
}

fn prompt_model_selection(current: &str) -> Option<String> {
    print_models(current);
    print!("  Select model [1-{}]: ", MODELS.len());
    io::stdout().flush().ok();

    let mut input = String::new();
    io::stdin().read_line(&mut input).ok()?;
    let input = input.trim();

    // Accept number
    if let Ok(n) = input.parse::<usize>() {
        if n >= 1 && n <= MODELS.len() {
            return Some(MODELS[n - 1].0.to_string());
        }
    }

    // Accept model ID directly
    if MODELS.iter().any(|(id, _)| *id == input) {
        return Some(input.to_string());
    }

    // Accept partial match
    let lower = input.to_lowercase();
    for (id, label) in MODELS {
        if id.to_lowercase().contains(&lower) || label.to_lowercase().contains(&lower) {
            return Some(id.to_string());
        }
    }

    println!("  Invalid selection.");
    None
}

// ============================================================
// Config subcommand functions (from bin/config.rs)
// ============================================================

fn mask_key(key: &str) -> String {
    if key.len() <= 8 {
        return "****".to_string();
    }
    format!("{}...{}", &key[..4], &key[key.len() - 4..])
}

fn config_prompt(label: &str, current: Option<&str>) -> String {
    if let Some(cur) = current {
        print!("{} [{}]: ", label, mask_key(cur));
    } else {
        print!("{}: ", label);
    }
    io::stdout().flush().ok();
    let mut input = String::new();
    io::stdin().read_line(&mut input).ok();
    let input = input.trim().to_string();
    if input.is_empty() {
        current.unwrap_or("").to_string()
    } else {
        input
    }
}

fn cmd_configure() {
    use llmshim::config;

    let mut cfg = config::load();
    println!("llmshim configuration");
    println!("Enter API keys (press Enter to keep current value)\n");

    let openai = config_prompt("OpenAI API Key", cfg.keys.openai.as_deref());
    if !openai.is_empty() {
        cfg.keys.openai = Some(openai);
    }

    let anthropic = config_prompt("Anthropic API Key", cfg.keys.anthropic.as_deref());
    if !anthropic.is_empty() {
        cfg.keys.anthropic = Some(anthropic);
    }

    let gemini = config_prompt("Gemini API Key", cfg.keys.gemini.as_deref());
    if !gemini.is_empty() {
        cfg.keys.gemini = Some(gemini);
    }

    let xai = config_prompt("xAI API Key", cfg.keys.xai.as_deref());
    if !xai.is_empty() {
        cfg.keys.xai = Some(xai);
    }

    let host = config_prompt("Proxy host", Some(&cfg.proxy.host));
    if !host.is_empty() {
        cfg.proxy.host = host;
    }

    let port_str = config_prompt("Proxy port", Some(&cfg.proxy.port.to_string()));
    if let Ok(port) = port_str.parse::<u16>() {
        cfg.proxy.port = port;
    }

    match config::save(&cfg) {
        Ok(()) => println!(
            "\nConfiguration saved to {}",
            config::config_path().display()
        ),
        Err(e) => {
            eprintln!("\nError saving config: {}", e);
            std::process::exit(1);
        }
    }
}

fn cmd_set(key: &str, value: &str) {
    use llmshim::config;

    let mut cfg = config::load();
    match key {
        "openai" => cfg.keys.openai = Some(value.to_string()),
        "anthropic" => cfg.keys.anthropic = Some(value.to_string()),
        "gemini" => cfg.keys.gemini = Some(value.to_string()),
        "xai" => cfg.keys.xai = Some(value.to_string()),
        "proxy.host" => cfg.proxy.host = value.to_string(),
        "proxy.port" => {
            cfg.proxy.port = value.parse().unwrap_or_else(|_| {
                eprintln!("Invalid port: {}", value);
                std::process::exit(1);
            });
        }
        _ => {
            eprintln!(
                "Unknown key: {}. Valid: openai, anthropic, gemini, xai, proxy.host, proxy.port",
                key
            );
            std::process::exit(1);
        }
    }
    match config::save(&cfg) {
        Ok(()) => println!(
            "Set {} = {}",
            key,
            if key.contains("proxy") {
                value.to_string()
            } else {
                mask_key(value)
            }
        ),
        Err(e) => {
            eprintln!("Error saving config: {}", e);
            std::process::exit(1);
        }
    }
}

fn cmd_get(key: &str) {
    let cfg = llmshim::config::load();
    let value = match key {
        "openai" => cfg.keys.openai.as_deref().map(mask_key),
        "anthropic" => cfg.keys.anthropic.as_deref().map(mask_key),
        "gemini" => cfg.keys.gemini.as_deref().map(mask_key),
        "xai" => cfg.keys.xai.as_deref().map(mask_key),
        "proxy.host" => Some(cfg.proxy.host.clone()),
        "proxy.port" => Some(cfg.proxy.port.to_string()),
        _ => {
            eprintln!("Unknown key: {}", key);
            std::process::exit(1);
        }
    };
    println!("{}", value.unwrap_or_else(|| "(not set)".to_string()));
}

fn cmd_list() {
    let cfg = llmshim::config::load();
    println!("Config: {}\n", llmshim::config::config_path().display());
    println!("API Keys:");
    for (name, value) in [
        ("openai", &cfg.keys.openai),
        ("anthropic", &cfg.keys.anthropic),
        ("gemini", &cfg.keys.gemini),
        ("xai", &cfg.keys.xai),
    ] {
        let display = match value {
            Some(v) if !v.is_empty() => mask_key(v),
            _ => "(not set)".to_string(),
        };
        println!("  {:12} {}", name, display);
    }
    println!("\nProxy:");
    println!("  {:12} {}", "host", cfg.proxy.host);
    println!("  {:12} {}", "port", cfg.proxy.port);
}

fn cmd_models() {
    llmshim::env::load_all();
    let router = llmshim::router::Router::from_env();
    let keys = router.provider_keys();
    let models = llmshim::models::available_models(&keys);
    for m in models {
        println!("  {} ({})", m.id, m.label);
    }
}

#[cfg(feature = "proxy")]
async fn cmd_proxy() {
    use std::net::SocketAddr;

    let router = llmshim::router::Router::from_env();
    let providers = router.provider_keys();
    if providers.is_empty() {
        eprintln!("No API keys found. Run: llmshim configure");
        std::process::exit(1);
    }

    let logger = std::env::var("LLMSHIM_LOG")
        .ok()
        .and_then(|path| llmshim::log::Logger::to_file(&path).ok());

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
    eprintln!("  POST /v1/chat · POST /v1/chat/stream · GET /v1/models · GET /health");

    let app = llmshim::proxy::app(router, logger);
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

// ============================================================
// Main entrypoint — subcommand dispatch
// ============================================================

fn print_global_usage() {
    eprintln!("llmshim — multi-provider LLM gateway\n");
    eprintln!("Usage: llmshim [command]\n");
    eprintln!("Commands:");
    eprintln!("  chat              Interactive chat (default)");
    eprintln!("  proxy             Start HTTP proxy server");
    eprintln!("  configure         Interactive API key setup");
    eprintln!("  set <key> <val>   Set a config value");
    eprintln!("  get <key>         Get a config value");
    eprintln!("  list              Show all configured keys");
    eprintln!("  models            List available models");
    eprintln!("  help              Show this help");
    eprintln!("\nRun 'llmshim' with no arguments to start chatting.");
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(|s| s.as_str()).unwrap_or("chat");

    match cmd {
        "configure" => {
            cmd_configure();
            return;
        }
        "set" => {
            if args.len() < 4 {
                eprintln!("Usage: llmshim set <key> <value>");
                std::process::exit(1);
            }
            cmd_set(&args[2], &args[3]);
            return;
        }
        "get" => {
            if args.len() < 3 {
                eprintln!("Usage: llmshim get <key>");
                std::process::exit(1);
            }
            cmd_get(&args[2]);
            return;
        }
        "list" | "ls" => {
            cmd_list();
            return;
        }
        "models" => {
            cmd_models();
            return;
        }
        "path" => {
            println!("{}", llmshim::config::config_path().display());
            return;
        }
        "help" | "--help" | "-h" => {
            print_global_usage();
            return;
        }
        "proxy" => {
            llmshim::env::load_all();
            #[cfg(feature = "proxy")]
            {
                cmd_proxy().await;
                return;
            }
            #[cfg(not(feature = "proxy"))]
            {
                eprintln!("Proxy not available. Rebuild with: cargo build --features proxy");
                std::process::exit(1);
            }
        }
        "chat" => { /* fall through to chat */ }
        _ if cmd.starts_with('-') => {
            print_global_usage();
            return;
        }
        _ => { /* unknown subcommand — treat as chat */ }
    }

    // --- Chat mode ---
    llmshim::env::load_all();
    let router = llmshim::router::Router::from_env();

    let log_path = args
        .iter()
        .position(|a| a == "--log")
        .and_then(|i| args.get(i + 1).cloned())
        .or_else(|| std::env::var("LLMSHIM_LOG").ok());
    let logger = match log_path {
        Some(path) => match Logger::to_file(&path) {
            Ok(l) => {
                eprintln!("  Logging to: {}", path);
                Some(l)
            }
            Err(e) => {
                eprintln!("  Warning: could not open log file {}: {}", path, e);
                None
            }
        },
        None => None,
    };

    println!("\n  llmshim — multi-provider LLM chat\n");
    let mut current_model = match prompt_model_selection("") {
        Some(m) => m,
        None => {
            println!("  Defaulting to Claude Sonnet 4.6");
            "anthropic/claude-sonnet-4-6".to_string()
        }
    };

    println!(
        "  Using: {} ({})",
        model_label(&current_model),
        current_model
    );
    println!("  Type /help for commands.\n");

    let mut messages: Vec<Value> = Vec::new();
    let mut pending_images: Vec<Value> = Vec::new();

    loop {
        // Prompt
        if pending_images.is_empty() {
            print!("\x1b[36myou\x1b[0m: ");
        } else {
            print!(
                "\x1b[36myou\x1b[0m \x1b[2m[{} image(s) attached]\x1b[0m: ",
                pending_images.len()
            );
        }
        io::stdout().flush().ok();

        let is_tty = std::io::IsTerminal::is_terminal(&std::io::stdin());
        let (input_text, input_content) = if is_tty {
            // Interactive terminal — raw mode for Ctrl-V image paste
            match read_line_raw() {
                Some(raw) => {
                    if raw.has_images {
                        // Interleaved text + images — use the blocks directly
                        // Also add any pending images from /image command
                        let mut all_blocks = raw.blocks;
                        all_blocks.append(&mut pending_images);
                        (raw.text, json!(all_blocks))
                    } else {
                        // Pure text — process inline file paths
                        let content = parse_input_with_images(&raw.text, &mut pending_images);
                        (raw.text, content)
                    }
                }
                None => break,
            }
        } else {
            // Piped stdin — regular line reading
            let mut line = String::new();
            if io::stdin().read_line(&mut line).is_err() || line.is_empty() {
                break;
            }
            let text = line.trim().to_string();
            let content = parse_input_with_images(&text, &mut pending_images);
            (text, content)
        };
        let input = input_text.trim();
        if input.is_empty() {
            continue;
        }

        // Commands
        match input {
            "/quit" | "/exit" | "/q" => break,
            "/help" | "/h" => {
                print_help();
                continue;
            }
            "/models" | "/model list" => {
                print_models(&current_model);
                continue;
            }
            "/clear" => {
                messages.clear();
                println!("  Conversation cleared.\n");
                continue;
            }
            "/history" => {
                println!("  {} messages in history.\n", messages.len());
                continue;
            }
            "/model" => {
                if let Some(m) = prompt_model_selection(&current_model) {
                    current_model = m;
                    println!(
                        "  Switched to: {} ({})\n",
                        model_label(&current_model),
                        current_model
                    );
                }
                continue;
            }
            _ if input.starts_with("/model ") => {
                let query = &input[7..].trim();
                // Accept number
                if let Ok(n) = query.parse::<usize>() {
                    if n >= 1 && n <= MODELS.len() {
                        current_model = MODELS[n - 1].0.to_string();
                        println!(
                            "  Switched to: {} ({})\n",
                            model_label(&current_model),
                            current_model
                        );
                        continue;
                    }
                }
                let lower = query.to_lowercase();
                let found = MODELS.iter().find(|(id, label)| {
                    id.to_lowercase().contains(&lower) || label.to_lowercase().contains(&lower)
                });
                if let Some((id, _)) = found {
                    current_model = id.to_string();
                    println!(
                        "  Switched to: {} ({})\n",
                        model_label(&current_model),
                        current_model
                    );
                } else {
                    println!("  Unknown model: {}\n", query);
                }
                continue;
            }
            "/paste" => {
                match clipboard_image() {
                    Some(block) => {
                        pending_images.push(block);
                        println!(
                            "  \x1b[32mImage pasted from clipboard ({} total attached)\x1b[0m\n",
                            pending_images.len()
                        );
                    }
                    None => {
                        println!("  No image found in clipboard.\n");
                    }
                }
                continue;
            }
            _ if input.starts_with("/image ") => {
                let path = input[7..].trim();
                match image_file_to_block(path) {
                    Some(block) => {
                        pending_images.push(block);
                        println!(
                            "  \x1b[32mAttached: {} ({} total)\x1b[0m\n",
                            path,
                            pending_images.len()
                        );
                    }
                    None => {
                        println!("  Could not read image: {}\n", path);
                    }
                }
                continue;
            }
            _ => {}
        }

        // Add user message (content already built with proper interleaving)
        messages.push(json!({"role": "user", "content": input_content}));

        // Build request
        let request = json!({
            "model": &current_model,
            "messages": messages,
            "max_tokens": 16384,
            "reasoning_effort": "high",
        });

        // Stream response
        print!("\x1b[33m{}\x1b[0m: ", model_label(&current_model));
        io::stdout().flush().ok();

        let timer = RequestTimer::start();
        let (provider_name, _) = current_model
            .split_once('/')
            .unwrap_or(("", &current_model));

        match llmshim::stream(&router, &request).await {
            Ok(mut stream) => {
                let mut full_text = String::new();
                let mut in_reasoning = false;
                let mut final_usage: Option<Value> = None;

                while let Some(chunk) = stream.next().await {
                    match chunk {
                        Ok(data) => {
                            let parsed: Value = serde_json::from_str(&data).unwrap_or_default();

                            // Reasoning tokens — dim grey
                            if let Some(reasoning) = parsed
                                .pointer("/choices/0/delta/reasoning_content")
                                .and_then(|c| c.as_str())
                            {
                                if !in_reasoning {
                                    print!("\x1b[2m\x1b[90m"); // dim + grey
                                    in_reasoning = true;
                                }
                                print!("{}", reasoning);
                                io::stdout().flush().ok();
                            }

                            // Content tokens
                            if let Some(text) = parsed
                                .pointer("/choices/0/delta/content")
                                .and_then(|c| c.as_str())
                            {
                                if in_reasoning {
                                    println!("\x1b[0m"); // reset, newline
                                    in_reasoning = false;
                                }
                                print!("{}", text);
                                io::stdout().flush().ok();
                                full_text.push_str(text);
                            }

                            // Capture usage from final chunk
                            if let Some(usage) = parsed.get("usage") {
                                final_usage = Some(usage.clone());
                            }
                        }
                        Err(e) => {
                            eprintln!("\n  Stream error: {}", e);
                            if let Some(ref logger) = logger {
                                logger.log(&LogEntry::from_error(
                                    provider_name,
                                    &current_model,
                                    &e.to_string(),
                                    timer.elapsed(),
                                ));
                            }
                            break;
                        }
                    }
                }
                if in_reasoning {
                    print!("\x1b[0m");
                }

                let elapsed = timer.elapsed();

                // Build final summary
                let usage = final_usage.unwrap_or(json!({}));
                let input_tok = usage
                    .get("prompt_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let output_tok = usage
                    .get("completion_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let reasoning_tok = usage
                    .get("reasoning_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);

                let mut summary = format!("{:.1}s", elapsed.as_secs_f32());
                if input_tok > 0 || output_tok > 0 {
                    summary.push_str(&format!(" · ↑ {} · ↓ {} tokens", input_tok, output_tok));
                    if reasoning_tok > 0 {
                        summary.push_str(&format!(" · reasoning {}", reasoning_tok));
                    }
                }

                // Print final summary inline after content
                println!("\n  \x1b[2m\x1b[90m[{}]\x1b[0m\n", summary);

                // Log to file
                if let Some(ref logger) = logger {
                    let log_resp = json!({"usage": usage, "id": ""});
                    logger.log(&LogEntry::from_response(
                        provider_name,
                        &current_model,
                        &log_resp,
                        elapsed,
                    ));
                }

                // Add assistant response to history
                if !full_text.is_empty() {
                    messages.push(json!({"role": "assistant", "content": full_text}));
                }
            }
            Err(e) => {
                let elapsed = timer.elapsed();
                eprintln!("  Error: {}\n", e);
                if let Some(ref logger) = logger {
                    logger.log(&LogEntry::from_error(
                        provider_name,
                        &current_model,
                        &e.to_string(),
                        elapsed,
                    ));
                }
                // Remove the failed user message
                messages.pop();
            }
        }
    }

    println!("  Bye!");
}
