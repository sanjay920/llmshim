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
    println!("    /clear         - clear conversation history");
    println!("    /history       - show message count");
    println!("    /quit          - exit");
    println!();
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

#[tokio::main]
async fn main() {
    // Load .env if present
    if let Ok(contents) = std::fs::read_to_string(".env") {
        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((key, value)) = line.split_once('=') {
                let value = value.trim();
                // Strip surrounding quotes
                let value = value
                    .strip_prefix('"')
                    .and_then(|v| v.strip_suffix('"'))
                    .unwrap_or(value);
                std::env::set_var(key.trim(), value);
            }
        }
    }

    let router = llmshim::router::Router::from_env();

    // Set up logger — write to llmshim.log if --log flag or LLMSHIM_LOG env var
    let log_path = std::env::args()
        .skip_while(|a| a != "--log")
        .nth(1)
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

    // Model selection
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

    loop {
        // Prompt
        print!("\x1b[36myou\x1b[0m: ");
        io::stdout().flush().ok();

        let mut input = String::new();
        if io::stdin().read_line(&mut input).is_err() || input.is_empty() {
            break;
        }
        let input = input.trim();
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
            _ => {}
        }

        // Add user message
        messages.push(json!({"role": "user", "content": input}));

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
