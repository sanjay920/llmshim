# llmshim

A blazing-fast LLM API translation layer written in **pure Rust**. One request format, every provider — OpenAI, Anthropic, Google Gemini, and xAI.

Send an OpenAI-style request, pick any model, and llmshim translates it to that provider's native API (and translates the response back). Switch providers by changing one string.

**Three ways to use it:**

| Surface | For | Install |
|---|---|---|
| **Rust crate** | Rust apps that call LLMs directly, in-process | `cargo add llmshim` |
| **CLI** | Interactive chat + a local proxy, from your terminal | `brew install sanjay920/tap/llmshim` |
| **HTTP proxy** | Any language (Python, JS, Go, …) over HTTP | run `llmshim proxy`, or `pip install llmshim` |

The Rust crate is the engine. The CLI and proxy wrap it. The Python package is a thin client that bundles the Rust binary, starts the proxy for you, and talks to it over HTTP — so you get the Rust engine behind a Python API.

## Benchmarks

Median of 5 runs, each p50 over 20 warm requests. Same prompt, same models, same machine.

| Metric | llmshim | litellm | langchain |
|---|---|---|---|
| Anthropic (p50) | 999ms | 981ms | **973ms** |
| OpenAI (p50) | **511ms** | 613ms | 602ms |
| Streaming TTFT | 1,023ms | **906ms** | 1,249ms |
| Memory (RSS) | **12 MB** | 281 MB | 281 MB |
| Transform overhead | **1.5µs** | — | — |

All three libraries hit the same APIs (Responses API for OpenAI, Messages API for Anthropic), so latency is dominated by the network round-trip — llmshim's own translation work is ~1.5µs, roughly a millionth of the request time.[^bench] The differences between libraries on any single latency row are within network noise and reshuffle run-to-run; the durable wins are memory footprint (~24× leaner than the Python stacks) and near-zero startup/overhead.

[^bench]: Because latency is >99.9% network, per-request p50s vary by more between runs of the same library than between libraries, which is why these are medians of 5 full runs rather than a single sample. Numbers were measured with `gpt-5.4` and `claude-sonnet-4-6`; your absolute values will differ by region and time of day.

Run it yourself:

```bash
cargo run --release --example bench        # Rust (llmshim)
uv run --with litellm --with langchain-anthropic --with langchain-openai \
  python benchmarks/bench_python.py        # Python (litellm + langchain)
```

## Configure API keys

llmshim reads keys from environment variables or `~/.llmshim/config.toml`. Precedence: **env vars > config file**.

```bash
export OPENAI_API_KEY=sk-...
export ANTHROPIC_API_KEY=sk-ant-...
export GEMINI_API_KEY=AIza...
export XAI_API_KEY=xai-...
```

Or persist them to the config file (used by all three surfaces):

```bash
llmshim configure          # interactive prompt
```

---

## Use it from Rust

```bash
cargo add llmshim tokio serde_json
```

```rust
use serde_json::json;

#[tokio::main]
async fn main() {
    // Router::from_env() picks up the *_API_KEY env vars.
    let router = llmshim::router::Router::from_env();

    let request = json!({
        "model": "claude-sonnet-5",
        "messages": [{"role": "user", "content": "What is Rust?"}],
        "max_tokens": 500,
    });

    // Responses come back in OpenAI Chat Completions format.
    let resp = llmshim::completion(&router, &request).await.unwrap();
    println!("{}", resp["choices"][0]["message"]["content"]);
}
```

Switch providers by changing the `"model"` string — everything else stays the same.

**Streaming:**

```rust
use futures::StreamExt;
use serde_json::json;

let router = llmshim::router::Router::from_env();
let request = json!({
    "model": "gpt-5.6-sol",
    "messages": [{"role": "user", "content": "Write a haiku about Rust."}],
    "max_tokens": 128,
});

let mut stream = llmshim::stream(&router, &request).await.unwrap();
while let Some(Ok(chunk)) = stream.next().await {
    let parsed: serde_json::Value = serde_json::from_str(&chunk).unwrap_or_default();
    if let Some(text) = parsed.pointer("/choices/0/delta/content").and_then(|c| c.as_str()) {
        print!("{text}");
    }
}
```

See [`examples/chat.rs`](examples/chat.rs) and [`examples/stream.rs`](examples/stream.rs) for runnable programs (`cargo run --example chat`).

---

## Use it from the CLI

```bash
brew install sanjay920/tap/llmshim        # macOS
cargo install llmshim --features proxy    # from source (any platform)
```

```bash
llmshim                     # show help
llmshim chat                # interactive multi-model chat (streaming, /model to switch)
llmshim configure           # set API keys
llmshim set <key> <value>   # set a config value
llmshim list                # show configured keys
llmshim models              # list available models
llmshim proxy               # start the HTTP proxy (see below)
```

---

## Use it from any language (HTTP proxy)

Run llmshim as a local HTTP server and call it from any language. It has its own compact API (not OpenAI-shaped).

```bash
llmshim proxy
# Listening on http://localhost:3000
```

```bash
curl http://localhost:3000/v1/chat \
  -H "Content-Type: application/json" \
  -d '{"model":"claude-sonnet-5","messages":[{"role":"user","content":"Hi"}],"config":{"max_tokens":100}}'
```

| Method | Path | Description |
|--------|------|-------------|
| `POST` | `/v1/chat` | Chat completion (or streaming with `stream: true`) |
| `POST` | `/v1/chat/stream` | Always-streaming SSE with typed events |
| `GET` | `/v1/models` | List available models |
| `GET` | `/health` | Health check |

Full API spec: [`api/openapi.yaml`](api/openapi.yaml).

### Scaling the proxy

The proxy is built to run as a horizontally-scaled fleet (Cloud Run, ECS, Kubernetes) without hammering provider rate limits. Two layers protect you:

- **Reactive retry** (always on): on an upstream 429/5xx it honors the provider's `Retry-After` header and reset hints, falling back to full-jitter exponential backoff.
- **Proactive shedding** (this layer): a per-provider token bucket rejects excess load *before* dispatching, and a per-instance concurrency cap sheds with 503 instead of running out of memory. Rejections carry a `Retry-After` header so clients back off cleanly.

All configuration is via env vars — everything optional with safe defaults. With no RPM/TPM limits set, only the concurrency cap applies.

| Var | Default | Meaning |
| --- | --- | --- |
| `LLMSHIM_MAX_CONCURRENCY` | `256` | Max in-flight upstream requests per instance. |
| `LLMSHIM_QUEUE_TIMEOUT_MS` | `5000` | Max wait for a concurrency slot before returning 503. |
| `LLMSHIM_RATE_LIMIT_RPM` | unset | Global requests-per-minute limit (per provider). |
| `LLMSHIM_RATE_LIMIT_TPM` | unset | Global tokens-per-minute limit. |
| `LLMSHIM_OPENAI_RPM`, `LLMSHIM_ANTHROPIC_TPM`, … | unset | Per-provider overrides (`LLMSHIM_<PROVIDER>_RPM`/`_TPM`). |
| `LLMSHIM_REDIS_URL` | unset | Enable distributed coordination (see below). |

Two deployment modes:

1. **Sidecar / zero-infra (default).** Each replica limits itself with an in-memory token bucket — no extra services. Running N replicas? Set each instance's limit to `provider_limit / N`.
2. **Redis-coordinated fleet.** Build with the `redis-coordination` feature and set `LLMSHIM_REDIS_URL`; all replicas share one global token bucket in Redis, so you can set the true provider limit once regardless of replica count. It fails open (keeps serving) if Redis is briefly unreachable.

```bash
# Zero-infra: cap each instance
LLMSHIM_MAX_CONCURRENCY=512 LLMSHIM_OPENAI_RPM=1000 llmshim proxy

# Redis-coordinated fleet (build with the feature once)
cargo build --release --features redis-coordination
LLMSHIM_REDIS_URL=redis://my-redis:6379 LLMSHIM_OPENAI_RPM=10000 llmshim proxy
```

Verify the shedding behavior yourself — the load-test harness drives the real proxy against a mock upstream (no provider calls, $0) and asserts the concurrency-cap, RPM-shed, and overload paths all shed correctly with `Retry-After`:

```bash
cargo run --release --features proxy --example loadtest
```

### Python client

`pip install llmshim` gives you a Python wrapper that bundles the Rust binary, starts the proxy on first use, and stops it on exit — no server to manage.

```bash
pip install llmshim
```

```python
import llmshim

# Keys can also come from env vars or `llmshim configure`.
llmshim.configure(anthropic="sk-ant-...", openai="sk-...")

resp = llmshim.chat("claude-sonnet-5", "Hello!", max_tokens=500)
print(resp["message"]["content"])
```

**Streaming:**

```python
for event in llmshim.stream("claude-sonnet-5", "Write a poem"):
    if event["type"] == "content":
        print(event["text"], end="", flush=True)
    elif event["type"] == "usage":
        print(f"\n[↑{event['input_tokens']} ↓{event['output_tokens']}]")
```

**Multi-model conversation** — switch providers mid-chat, history carries over:

```python
messages = [{"role": "user", "content": "What is a closure?"}]

r1 = llmshim.chat("claude-sonnet-5", messages, max_tokens=500)
print(f"Claude: {r1['message']['content']}")

messages.append({"role": "assistant", "content": r1["message"]["content"]})
messages.append({"role": "user", "content": "Now explain it differently."})

r2 = llmshim.chat("gpt-5.6-sol", messages, max_tokens=500)
print(f"GPT: {r2['message']['content']}")
```

**Tool use** — pass tools in OpenAI Chat Completions format; llmshim translates to each provider's native format:

```python
tools = [{
    "type": "function",
    "function": {
        "name": "get_weather",
        "description": "Get current weather",
        "parameters": {
            "type": "object",
            "properties": {"city": {"type": "string"}},
            "required": ["city"],
        },
    },
}]

resp = llmshim.chat("claude-sonnet-5", "Weather in Tokyo?", max_tokens=500, tools=tools)
for tc in resp["message"].get("tool_calls", []):
    print(f"{tc['function']['name']}({tc['function']['arguments']})")
```

**Reasoning / thinking** — one vocabulary across every provider:

```python
resp = llmshim.chat(
    "claude-sonnet-5",
    "Solve: x^2 - 5x + 6 = 0",
    max_tokens=4000,
    reasoning_effort="high",   # none | low | medium | high | xhigh | max
    reasoning_mode="pro",      # standard (default) | pro — much more model work
)
print(resp["reasoning"])          # thinking content
print(resp["message"]["content"]) # answer
```

llmshim maps these to each provider's native control (OpenAI `reasoning.effort`/`mode`, Anthropic adaptive thinking, Gemini `thinkingLevel`, xAI `reasoning.effort`), clamping to the nearest tier the target model actually supports — so `reasoning_effort="max"` works everywhere even though only some models have a native `max`. Full verified mapping tables: [the reasoning guide](https://sanjay920.github.io/llmshim/guides/reasoning.html). Prefer a provider's exact native dialect? Pass it via `provider_config` (`x-openai.reasoning`, `x-anthropic.thinking`, `x-gemini.thinkingConfig`) and llmshim won't touch it.

**Fallback chains** — automatic failover across providers:

```python
resp = llmshim.chat(
    "anthropic/claude-sonnet-5",
    "Hello",
    max_tokens=100,
    fallback=["openai/gpt-5.6-sol", "gemini/gemini-3.5-flash"],
)
```

> These capabilities (streaming, multi-model, tools, reasoning, fallback) are all provided by the Rust core, so they work identically from the Rust crate and the proxy — the Python snippets above are just the most concise way to show them.

### TypeScript / JavaScript client

`npm install llmshim` bundles a prebuilt proxy binary for your platform (same idea as the Python package) and auto-starts it on first use — nothing to run yourself. Pass an explicit `baseUrl` instead to connect to a proxy you're already running.

```bash
npm install llmshim
```

```typescript
import { Client } from "llmshim";

const client = new Client(); // no baseUrl -> auto-starts the bundled proxy
const res = await client.chat({
  model: "anthropic/claude-sonnet-5",
  messages: [{ role: "user", content: "Hello!" }],
});
console.log(res.message.content);
```

Full docs: [`clients/typescript/README.md`](clients/typescript/README.md).

### Go client

```bash
go get github.com/sanjay920/llmshim/clients/go
```

```go
client := llmshim.New() // defaults to http://localhost:3000
resp, err := client.Chat(ctx, llmshim.ChatRequest{
    Model:    "anthropic/claude-sonnet-5",
    Messages: []llmshim.Message{{Role: "user", Content: "Hello!"}},
})
```

Standard library only. Full docs: [`clients/go/README.md`](clients/go/README.md).

### Ruby client

```bash
gem install llmshim
```

```ruby
require "llmshim"

resp = Llmshim.chat(model: "anthropic/claude-sonnet-5", messages: [{role: "user", content: "Hello!"}])
puts resp.message.content
```

Standard library only. Full docs: [`clients/ruby/README.md`](clients/ruby/README.md).

---

## Supported models

| Provider | Models | Reasoning visible |
|----------|--------|-------------------|
| **OpenAI** | `gpt-5.6-sol`, `gpt-5.6-terra`, `gpt-5.6-luna`, `gpt-5.5`, `gpt-5.5-pro`, `gpt-5.4`, `gpt-5.4-pro`, `gpt-5.4-mini`, `gpt-5.4-nano` | Yes (summaries) |
| **Anthropic** | `claude-opus-4-8`, `claude-sonnet-5`, `claude-opus-4-7`, `claude-opus-4-6`, `claude-sonnet-4-6`, `claude-haiku-4-5-20251001` | Yes (full thinking) |
| **Google Gemini** | `gemini-3.5-flash`, `gemini-3.1-pro-preview`, `gemini-3-flash-preview` | Yes (thought summaries) |
| **xAI** | `grok-4.5`, `grok-4.3`, `grok-4.20-multi-agent-beta-0309`, `grok-4.20-beta-0309-reasoning`, `grok-4.20-beta-0309-non-reasoning` | No (hidden) |

Use a bare model name (auto-detected by prefix) or an explicit `provider/model` string.

## Docker

```bash
llmshim docker build
llmshim docker start
llmshim docker status
llmshim docker logs
llmshim docker stop
```

## How it works

No canonical struct. Requests flow as `serde_json::Value` — each provider maps only what it understands. Adding a provider = implementing one trait with three methods.

```
llmshim::completion(router, request)
  → router.resolve("anthropic/claude-sonnet-5")
  → provider.transform_request(model, &value)
  → HTTP
  → provider.transform_response(model, body)
```

## Key features

- **Multi-model conversations** — switch providers mid-chat, history carries over
- **Reasoning/thinking** — visible chain-of-thought from OpenAI, Anthropic, and Gemini
- **Streaming** — token-by-token, with thinking surfaced separately
- **Tool use** — Chat Completions format auto-translated to each provider
- **Vision/images** — send images in any format, auto-translated between providers
- **Fallback chains** — automatic failover across providers with exponential backoff
- **Cross-provider translation** — system messages, tool calls, and provider-specific fields all handled

## Build & test

```bash
cargo build                                    # dev build
cargo build --release --features proxy         # release build (~6MB binary)
cargo test --features proxy --tests            # unit tests (~370)
cargo test --features proxy -- --ignored       # integration tests (needs API keys)
```
