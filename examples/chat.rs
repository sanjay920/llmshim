use serde_json::json;

#[tokio::main]
async fn main() {
    let router = llmshim::router::Router::from_env()
        .alias("smart", "anthropic/claude-sonnet-4-6")
        .alias("fast", "openai/gpt-5.4");

    // Use an alias
    let request = json!({
        "model": "smart",
        "messages": [
            {"role": "system", "content": "You are a helpful assistant. Be concise."},
            {"role": "user", "content": "What is the capital of France?"}
        ],
        "max_tokens": 128,
    });

    match llmshim::completion(&router, &request).await {
        Ok(resp) => {
            println!("{}", serde_json::to_string_pretty(&resp).unwrap());
        }
        Err(e) => eprintln!("Error: {}", e),
    }
}
