# llmshim (Ruby client)

A thin, dependency-free Ruby client for the [llmshim](https://github.com/sanjay920/llmshim)
multi-provider LLM proxy. Send OpenAI-style chat requests to a running llmshim
proxy and let it translate to OpenAI, Anthropic, Google Gemini, xAI, OpenRouter,
vLLM, or SGLang.

This gem talks to a proxy over plain HTTP — it does **not** spawn the Rust
binary. Start the proxy separately (`llmshim proxy`, default `http://localhost:3000`).

- Standard library only (`net/http`, `json`, `uri`) — no runtime gem dependencies.
- Non-streaming and SSE streaming chat, model listing, health check.
- Typed responses and a typed `Llmshim::APIError`.

## Install

From RubyGems (once published):

```bash
gem install llmshim
```

Or add to your `Gemfile`:

```ruby
gem "llmshim"
```

Or build locally from this directory:

```bash
gem build llmshim.gemspec
gem install ./llmshim-*.gem
```

## Prerequisite: run the proxy

```bash
llmshim proxy            # listens on 0.0.0.0:3000 by default
```

Configure provider credentials for the proxy via `llmshim configure` or
environment variables. The Ruby client never sees your keys — the proxy holds them.

| Provider   | Model string form                       | Env vars                              |
| ---------- | --------------------------------------- | ------------------------------------- |
| OpenAI     | `openai/gpt-5.6-sol`                    | `OPENAI_API_KEY`                      |
| Anthropic  | `anthropic/claude-sonnet-5`            | `ANTHROPIC_API_KEY`                   |
| Gemini     | `gemini/gemini-3.5-flash`              | `GEMINI_API_KEY`                      |
| xAI        | `xai/grok-4.5`                         | `XAI_API_KEY`                         |
| OpenRouter | `openrouter/anthropic/claude-sonnet-4.5` | `OPENROUTER_API_KEY`                |
| vLLM       | `vllm/<served-model>`                  | `VLLM_BASE_URL` (+ optional `VLLM_API_KEY`)   |
| SGLang     | `sglang/<served-model>`                | `SGLANG_BASE_URL` (+ optional `SGLANG_API_KEY`) |

vLLM and SGLang are self-hosted (local or remote) OpenAI-compatible servers;
point the proxy at them with `VLLM_BASE_URL` / `SGLANG_BASE_URL`.

## Quickstart

```ruby
require "llmshim"

client = Llmshim::Client.new(base_url: "http://localhost:3000")

resp = client.chat(model: "anthropic/claude-sonnet-5", messages: "What is Rust?")
puts resp.content                 # => "Rust is a systems programming language..."
puts resp.provider                # => "anthropic"
puts resp.latency_ms              # => 1234 (round-trip latency in ms)
puts resp.usage.total_tokens
puts resp.usage.reasoning_tokens  # thinking tokens billed (nil if none)
```

`messages:` accepts a single string (treated as one user message) or an array
of message hashes:

```ruby
resp = client.chat(
  model: "openai/gpt-5.5",
  messages: [
    { role: "system", content: "You are a pirate." },
    { role: "user",   content: "Hello!" }
  ],
  max_tokens: 500,
  temperature: 0.7,
  reasoning_effort: "high"
)
```

### Streaming vs. non-streaming

`chat` is non-streaming and returns a single `Llmshim::ChatResponse`. Passing
`stream: true` to `chat` raises `ArgumentError` — the proxy would emit SSE that
`chat` cannot parse. Use `stream` (below) for token-by-token output.

### Module-level convenience

A shared default client (base URL from `LLMSHIM_BASE_URL`, else `http://localhost:3000`):

```ruby
require "llmshim"

resp = Llmshim.chat(model: "gpt-5.5", messages: "Explain quicksort")
puts resp.content
```

## Streaming

`stream` yields a `Llmshim::StreamEvent` for each SSE event and stops after the
`done` event. Event types: `content`, `reasoning`, `tool_call`, `usage`,
`done`, `error`.

```ruby
client.stream(model: "anthropic/claude-sonnet-5", messages: "Write a haiku") do |event|
  case event.type
  when "reasoning" then print event.text   # thinking tokens
  when "content"   then print event.text   # answer tokens
  when "tool_call" then puts "\n[tool] #{event.name}(#{event.arguments})"
  when "usage"     then puts "\ntokens: #{event.usage.total_tokens}"
  when "error"     then warn "error: #{event.message}"
  end
end
```

Predicate helpers are available too: `event.content?`, `event.reasoning?`,
`event.tool_call?`, `event.usage?`, `event.done?`, `event.error?`.

Called without a block, `stream` returns the collected array of events:

```ruby
events = client.stream(model: "gpt-5.5", messages: "Hi")
text = events.select(&:content?).map(&:text).join
```

## Tools, provider passthrough, and fallback

```ruby
resp = client.chat(
  model: "anthropic/claude-sonnet-5",
  messages: "What's the weather in SF?",
  tools: [
    { type: "function",
      function: { name: "get_weather",
                  parameters: { type: "object",
                                properties: { city: { type: "string" } } } } }
  ],
  tool_choice: "auto",
  # Provider-specific controls, namespaced under x-<provider> (see below):
  provider_config: { "x-anthropic" => { thinking: { type: "enabled", budget_tokens: 4000 } } },
  # Try these models if the primary fails with a retryable error:
  fallback: ["openai/gpt-5.6-sol", "gemini/gemini-3.5-flash"]
)

resp.message.tool_calls.each do |tc|
  puts "#{tc.name} -> #{tc.arguments}"
end
```

`tools` and `tool_choice` are folded into `provider_config` (passed straight
through to the provider). `max_tokens`, `temperature`, `top_p`, `top_k`,
`stop`, `reasoning_effort`, and `reasoning_mode` are folded into the request
`config`.

### Provider passthrough (`provider_config`)

`provider_config` merges at the **request root**, so native provider controls
must be namespaced under an `x-<provider>` key — a bare `thinking:` at the top
level is ignored. Use the namespace matching the target provider:

| Provider   | Namespace       |
| ---------- | --------------- |
| OpenAI     | `x-openai`      |
| Anthropic  | `x-anthropic`   |
| Gemini     | `x-gemini`      |
| OpenRouter | `x-openrouter`  |
| vLLM       | `x-vllm`        |
| SGLang     | `x-sglang`      |

```ruby
# Anthropic extended thinking:
provider_config: { "x-anthropic" => { thinking: { type: "enabled", budget_tokens: 4000 } } }

# OpenRouter provider routing (also accepts `models`, `transforms`):
provider_config: { "x-openrouter" => { provider: { sort: "throughput" } } }
```

`provider_config` also carries a few root-level keys the proxy understands
directly (not namespaced):

- `tools` / `tool_choice` — folded in for you from the `tools:`/`tool_choice:` kwargs.
- `response_format` — e.g. `{ type: "json_object" }` for structured output.
- `reasoning_summary` — request a summary of the model's reasoning.

```ruby
provider_config: {
  response_format: { type: "json_object" },
  reasoning_summary: "auto"
}
```

### Reasoning controls

Two provider-agnostic knobs live in `config` (pass them as top-level kwargs):

- `reasoning_effort` — one of `none`, `low`, `medium`, `high`, `xhigh`, `max`.
  Clamped to the nearest tier the target model supports.
- `reasoning_mode` — `standard` or `pro`. `pro` is native on OpenAI gpt-5.6/-pro
  models and emulated as a one-tier effort bump elsewhere.

```ruby
resp = client.chat(
  model: "openai/gpt-5.6-sol",
  messages: "Prove that sqrt(2) is irrational.",
  reasoning_effort: "high",
  reasoning_mode: "pro"
)
```

## Models and health

```ruby
client.models.each { |m| puts "#{m.id} (#{m.provider})" }

h = client.health
puts h.status          # => "ok"
puts h.providers       # => ["anthropic", "openai", ...]
```

## Error handling

Any non-2xx response raises `Llmshim::APIError`, populated from the proxy's
`ErrorResponse` body:

```ruby
begin
  client.chat(model: "bogus/model", messages: "hi")
rescue Llmshim::APIError => e
  warn "#{e.status} #{e.code}: #{e.message}"
end
```

`Llmshim::APIError` exposes `#status` (Integer), `#code` (String or nil),
`#message`, and `#body` (raw response body).

## Configuration

`Llmshim::Client.new` options:

| Option     | Default                  | Description                          |
| ---------- | ------------------------ | ------------------------------------ |
| `base_url` | `http://localhost:3000`  | Proxy base URL                       |
| `headers`  | `{}`                     | Extra headers sent on every request  |
| `timeout`  | `120`                    | Open/read timeout in seconds         |

## Development

```bash
bundle install     # optional; only for dev tools
rake test          # runs the mocked test suite (no network, no API keys)
```

Tests use a local WEBrick mock server returning canned JSON and SSE — they
never contact a real provider and never require a running proxy.

## License

MIT
