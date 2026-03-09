use llmshim::config;
use std::io::{self, Write};

fn print_usage() {
    eprintln!("llmshim config — manage API keys and settings");
    eprintln!();
    eprintln!("Usage:");
    eprintln!("  llmshim-config configure          Interactive setup (like aws configure)");
    eprintln!("  llmshim-config set <key> <value>   Set a config value");
    eprintln!("  llmshim-config get <key>           Get a config value");
    eprintln!("  llmshim-config list                Show all configured keys");
    eprintln!("  llmshim-config path                Show config file path");
    eprintln!();
    eprintln!("Keys: openai, anthropic, gemini, xai, proxy.host, proxy.port");
}

fn mask_key(key: &str) -> String {
    if key.len() <= 8 {
        return "****".to_string();
    }
    format!("{}...{}", &key[..4], &key[key.len() - 4..])
}

fn prompt(label: &str, current: Option<&str>) -> String {
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

fn configure() {
    let mut cfg = config::load();

    println!("llmshim configuration");
    println!("Enter API keys (press Enter to keep current value)\n");

    let openai = prompt("OpenAI API Key", cfg.keys.openai.as_deref());
    if !openai.is_empty() {
        cfg.keys.openai = Some(openai);
    }

    let anthropic = prompt("Anthropic API Key", cfg.keys.anthropic.as_deref());
    if !anthropic.is_empty() {
        cfg.keys.anthropic = Some(anthropic);
    }

    let gemini = prompt("Gemini API Key", cfg.keys.gemini.as_deref());
    if !gemini.is_empty() {
        cfg.keys.gemini = Some(gemini);
    }

    let xai = prompt("xAI API Key", cfg.keys.xai.as_deref());
    if !xai.is_empty() {
        cfg.keys.xai = Some(xai);
    }

    let host = prompt("Proxy host", Some(&cfg.proxy.host));
    if !host.is_empty() {
        cfg.proxy.host = host;
    }

    let port_str = prompt("Proxy port", Some(&cfg.proxy.port.to_string()));
    if let Ok(port) = port_str.parse::<u16>() {
        cfg.proxy.port = port;
    }

    match config::save(&cfg) {
        Ok(()) => {
            println!(
                "\nConfiguration saved to {}",
                config::config_path().display()
            );
        }
        Err(e) => {
            eprintln!("\nError saving config: {}", e);
            std::process::exit(1);
        }
    }
}

fn set_value(key: &str, value: &str) {
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
            eprintln!("Unknown key: {}", key);
            eprintln!("Valid keys: openai, anthropic, gemini, xai, proxy.host, proxy.port");
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

fn get_value(key: &str) {
    let cfg = config::load();
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
    match value {
        Some(v) => println!("{}", v),
        None => println!("(not set)"),
    }
}

fn list_config() {
    let cfg = config::load();
    let path = config::config_path();
    println!("Config: {}\n", path.display());

    let keys = [
        ("openai", &cfg.keys.openai),
        ("anthropic", &cfg.keys.anthropic),
        ("gemini", &cfg.keys.gemini),
        ("xai", &cfg.keys.xai),
    ];
    println!("API Keys:");
    for (name, value) in keys {
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

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 {
        print_usage();
        std::process::exit(1);
    }

    match args[1].as_str() {
        "configure" => configure(),
        "set" => {
            if args.len() < 4 {
                eprintln!("Usage: llmshim-config set <key> <value>");
                std::process::exit(1);
            }
            set_value(&args[2], &args[3]);
        }
        "get" => {
            if args.len() < 3 {
                eprintln!("Usage: llmshim-config get <key>");
                std::process::exit(1);
            }
            get_value(&args[2]);
        }
        "list" | "ls" => list_config(),
        "path" => println!("{}", config::config_path().display()),
        "help" | "--help" | "-h" => print_usage(),
        other => {
            eprintln!("Unknown command: {}", other);
            print_usage();
            std::process::exit(1);
        }
    }
}
