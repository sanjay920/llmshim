# llmshim

A blazing-fast LLM API translation layer written in **pure Rust**. One request format, every provider — OpenAI, ChatGPT subscriptions, Anthropic, Google Gemini, xAI, OpenRouter, and self-hosted vLLM / SGLang.

Send an OpenAI-style request, pick any model, and llmshim translates it to that provider's native API (and translates the response back). Switch providers by changing one string.

**Three ways to use it:**

| Surface | For | Install |
|---|---|---|
| **Rust crate** | Rust apps that call LLMs directly, in-process | `cargo add llmshim` |
| **CLI** | Interactive chat + a local proxy, from your terminal | `brew install sanjay920/tap/llmshim` |
| **HTTP proxy** | Any language (Python, JS, Go, …) over HTTP | run `llmshim proxy`, or `pip install llmshim` |

The Rust crate is the engine. The CLI and proxy wrap it. The Python package is a thin client that bundles the Rust binary, starts the proxy for you, and talks to it over HTTP — so you get the Rust engine behind a Python API.

## Benchmarks

Measured on **2026-09-18 UTC** (September 17 Pacific), Apple M5 Pro,
macOS 26.4.1, Rust 1.97.0, llmshim 0.4.0 in release mode. These are measurements
of the current Rust implementation.

The API run used `claude-sonnet-4-6` and `gpt-5.4`, the prompt
`Say 'benchmark' and nothing else.`, a 50-token output cap, no tools, and
`reasoning_effort: none`. Each warm row summarizes 20 complete responses from
one run; p50 uses the upper median. The streaming row is one completed stream.

| Metric | Measured result |
|---|---:|
| Anthropic first request, after connection warmup | 1426.84 ms |
| Anthropic warm p50 / mean | 934.64 / 1111.58 ms |
| OpenAI warm p50 / mean | 731.79 / 790.89 ms |
| Anthropic streaming time to first visible text | 872.01 ms |
| Process RSS after API calls, before the tool sweep | 85.48 MiB |

A separate local benchmark measures request transformation with distinct tool
parameter schemas reused across 10,000 iterations. It includes request copying,
replay/cache policy, schema handling, and native translation. HTTP serialization,
dispatch, and the higher-level capability plan are outside this measurement.
The full synthetic fixture is in [`benchmarks/bench.rs`](benchmarks/bench.rs).

| Tools | Schema cache disabled | Warm schema cache |
|---:|---:|---:|
| 0 | 6.48 µs | 5.88 µs |
| 1 | 17.09 µs | 10.01 µs |
| 5 | 61.44 µs | 25.55 µs |
| 10 | 116.74 µs | 45.03 µs |
| 25 | 277.37 µs | 106.01 µs |
| 50 | 559.19 µs | 210.39 µs |

CPU values are means from separate runs; small differences, including the
zero-tool row, reflect run-to-run variation.

At 50 tools, memoization makes this full transform **2.66× faster**.
The remaining 210.39 µs includes hashing and copying; caching removes repeated
schema walking and validation compilation. For scale, that is about
**0.023%** of the measured one-word, zero-tool Anthropic response time above.
End-to-end latency includes network and model work, and varies with payload,
provider load, region, and time of day.

Run it yourself:

```bash
# Reads ANTHROPIC_API_KEY and OPENAI_API_KEY from env or ~/.llmshim/config.toml.
# Makes 43 logical model requests; the normal client retry policy applies.
cargo run --release --example bench

# Local CPU measurements; no keys or HTTP requests.
cargo run --release --example bench -- --transforms-only
LLMSHIM_NO_SCHEMA_CACHE=1 cargo run --release --example bench -- --transforms-only
```

## Configure API keys

llmshim reads keys from environment variables or `~/.llmshim/config.toml`. Precedence: **env vars > config file**.

```bash
export OPENAI_API_KEY=sk-...
export ANTHROPIC_API_KEY=sk-ant-...
export GEMINI_API_KEY=AIza...
export XAI_API_KEY=xai-...
export OPENROUTER_API_KEY=sk-or-...
```

Reach any model through [OpenRouter](https://openrouter.ai) by addressing it as
`openrouter/<vendor>/<model>` (e.g. `openrouter/anthropic/claude-sonnet-5`).
OpenRouter is OpenAI Chat Completions-compatible, so tools, vision, streaming,
and `reasoning_effort` all pass through; OpenRouter-only controls (provider
routing, model fallbacks, transforms) go under an `x-openrouter` key.

OpenRouter also reports **what it actually charged**, and llmshim asks for that
by default (`usage: {include: true}`). The response then carries
`usage.cost_usd` with `usage.cost_source: "provider"` — the bill, not the
catalog estimate — alongside `usage.cost_details` and `usage.is_byok`. Send
your own `usage` object to change or disable it. Because OpenRouter is an
aggregator, the response's top-level **`provider`** names the upstream that
actually served the call (`"Together"`, `"Fireworks"`, …) and `id` is the
OpenRouter generation id; both survive streaming and buffered paths. On a
stream the accounting rides on the terminal chunk, which OpenRouter only sends
when you set `stream_options: {include_usage: true}`.

Point at a **self-hosted vLLM or SGLang** server (local or remote) by setting its
base URL — no key needed unless the server was launched with one:

```bash
export SGLANG_BASE_URL=http://localhost:30000/v1   # or https://your-host/v1
export VLLM_BASE_URL=http://localhost:8000/v1
```

Then address the served model as `sglang/<served-model>` or `vllm/<served-model>`
(e.g. `sglang/Qwen/Qwen3.6-35B-A3B-FP8`). Server-specific knobs go under
`x-vllm` / `x-sglang`.

A server that also serves `/v1/responses` (SGLang does) can be spoken to on
that wire with `SGLANG_WIRE=responses` (or `VLLM_WIRE=responses`). Reasoning
then comes back as an item with its own id rather than bare `reasoning_content`,
and is replayed as that item. Replay is gated on a known model family on every
wire, and the public catalog does not know a served model: declare it once in
`.llmshim/models.toml` —

```toml
[models."sglang/<served-model>"]
family = "qwen"
```

Or persist them to the config file (used by all three surfaces):

```bash
llmshim configure          # interactive prompt
```

---

## ChatGPT subscription (OAuth)

Sign in once with a ChatGPT account, then use `chatgpt/<model>` from Rust,
the CLI, or any proxy client. No `OPENAI_API_KEY` is needed for this route.
The device-code flow follows [LiteLLM's ChatGPT provider](https://docs.litellm.ai/docs/providers/chatgpt).

```bash
llmshim login chatgpt             # open the printed URL and enter the code
llmshim login chatgpt --status    # inspect the saved login without network access
llmshim chat                     # select a ChatGPT model
# or: llmshim proxy
```

If needed, enable device-code login in your ChatGPT security settings or
workspace permissions ([OpenAI authentication docs](https://learn.chatgpt.com/docs/auth#preferred-device-code-authentication-beta)).

```json
{
  "model": "chatgpt/gpt-6-astra",
  "messages": [{"role": "user", "content": "Hello!"}]
}
```

Tokens live in `~/.llmshim/chatgpt/auth.json`; expired access tokens refresh
automatically, with file locking and atomic saves. `CHATGPT_TOKEN_DIR` and
`CHATGPT_AUTH_FILE` select a different cache (LiteLLM's flat auth-file format
is supported). This cache is independent of Codex's login. Run
`llmshim logout chatgpt` to remove the selected local cache; this does not
revoke the session at OpenAI. Login is explicit, never started inside a proxy
request. Restart an existing proxy after the first login so it registers the
provider. For containers, mount the token directory writable and set
`CHATGPT_TOKEN_DIR` to its container path.

Both completion and streaming calls use the subscription Responses backend.
Non-streaming calls collect the upstream stream into one normal response.
Tools, images, and reasoning use the existing Responses translation;
`x-chatgpt` supplies supported native fields (under `provider_config` in proxy
requests). The backend requires `store: false` and `stream: true` and rejects
token limits, sampling fields, and metadata, so these constraints also apply
to native overrides. Bare `gpt-*` names still route to API-key OpenAI.
The ChatGPT route supports only `chatgpt/gpt-6-astra`, `chatgpt/gpt-5.6-sol`,
`chatgpt/gpt-5.6-terra`, and `chatgpt/gpt-5.6-luna`. Older and unlisted model
IDs return a local error before authentication or an upstream request.
Access to these models and usage limits depend on the ChatGPT account.

See [provider configuration](docs/src/reference/configuration.md#chatgpt-oauth)
for endpoint and header overrides.

## Endpoint redirects

The shared HTTP client does not follow redirects. Configure the final API URL
directly: a 3xx response is returned as a provider error instead of forwarding
the prompt and provider-specific credential headers to another endpoint.
This applies to both streaming and non-streaming requests.

## Use it from Rust

Non-streaming OpenAI Responses, xAI Responses, Gemini, and Anthropic results
require supported terminal metadata. Missing, malformed, or unsupported
statuses return a `ProviderError` (502) with a fixed diagnostic instead of
being interpreted as successful completion. Known completion, output-limit,
filtering and Anthropic tool-call mappings remain available. Additional native
reasons require an explicit mapping; streaming transforms are unchanged.
Provider mocks should include the native terminal status/stop reason.

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
llmshim login chatgpt        # sign in with a ChatGPT subscription
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
    fallback=["openai/gpt-5.6-sol", "gemini/gemini-3.8-flash"],
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

## Advertised models

| Provider | Models | Reasoning visible |
|----------|--------|-------------------|
| **OpenAI** | `gpt-6-astra`, `gpt-5.6-sol`, `gpt-5.6-terra`, `gpt-5.6-luna` | Yes (summaries) |
| **Anthropic** | `claude-fable-5-1`, `claude-opus-5`, `claude-sonnet-5`, `claude-haiku-4-5-20251001` | Yes (thinking summaries) |
| **Google Gemini** | `gemini-3.8-flash`, `gemini-3.5-flash-lite` | Yes (thought summaries) |
| **xAI** | `grok-4.7` | No (hidden) |

The CLI and server advertise these current tiers. ChatGPT subscription access
uses the same four OpenAI models under `chatgpt/`. OpenRouter and self-hosted
providers accept caller-selected IDs without a fixed advertised list.

Use a bare model name (auto-detected by prefix) or an explicit `provider/model`
string. Older explicit IDs retain their provider routing and metadata; the
ChatGPT route continues to enforce its four-model allowlist.

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

## Contributing

Contributions are welcome — see [CONTRIBUTING.md](CONTRIBUTING.md) for the
development setup, the CI gates, and the rules that protect the public API.
Report suspected vulnerabilities privately per [SECURITY.md](SECURITY.md), and
be excellent to each other ([Code of Conduct](CODE_OF_CONDUCT.md)).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option. llmshim is provided **"AS IS",
without warranty of any kind**; the authors and contributors accept no
liability for its use. See [NOTICE](NOTICE). Unless you explicitly state
otherwise, any contribution intentionally submitted for inclusion in llmshim by
you, as defined in the Apache-2.0 license, shall be dual licensed as above,
without any additional terms or conditions.
