use futures::StreamExt;
use serde_json::json;

#[tokio::main]
async fn main() {
    let router = llmshim::router::Router::from_env();

    let models = ["anthropic/claude-sonnet-4-20250514", "openai/gpt-4o"];

    for model in models {
        println!("\n--- streaming: {} ---", model);
        let request = json!({
            "model": model,
            "messages": [
                {"role": "user", "content": "Write a haiku about Rust programming."}
            ],
            "max_tokens": 128,
        });

        match llmshim::stream(&router, &request).await {
            Ok(mut stream) => {
                while let Some(chunk) = stream.next().await {
                    match chunk {
                        Ok(data) => {
                            let parsed: serde_json::Value =
                                serde_json::from_str(&data).unwrap_or_default();
                            if let Some(text) = parsed
                                .pointer("/choices/0/delta/content")
                                .and_then(|c| c.as_str())
                            {
                                print!("{}", text);
                            }
                        }
                        Err(e) => {
                            eprintln!("\nStream error: {}", e);
                            break;
                        }
                    }
                }
                println!();
            }
            Err(e) => eprintln!("Error: {}", e),
        }
    }
}
