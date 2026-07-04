# llmshim (Ruby client)

A thin, dependency-free Ruby client for the [llmshim](https://github.com/sanjay920/llmshim)
multi-provider LLM proxy. Send OpenAI-style chat requests to a running llmshim
proxy and let it translate to OpenAI, Anthropic, Google Gemini, or xAI.

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

Configure provider API keys for the proxy via `llmshim configure` or the
`OPENAI_API_KEY` / `ANTHROPIC_API_KEY` / `GEMINI_API_KEY` / `XAI_API_KEY`
environment variables. The Ruby client never sees your keys — the proxy holds them.

## Quickstart

```ruby
require "llmshim"

client = Llmshim::Client.new(base_url: "http://localhost:3000")

resp = client.chat(model: "claude-sonnet-4-6", messages: "What is Rust?")
puts resp.content                 # => "Rust is a systems programming language..."
puts resp.provider                # => "anthropic"
puts resp.usage.total_tokens
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
client.stream(model: "claude-sonnet-4-6", messages: "Write a haiku") do |event|
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
  model: "anthropic/claude-sonnet-4-6",
  messages: "What's the weather in SF?",
  tools: [
    { type: "function",
      function: { name: "get_weather",
                  parameters: { type: "object",
                                properties: { city: { type: "string" } } } } }
  ],
  tool_choice: "auto",
  # Raw provider-specific JSON merged into the underlying request:
  provider_config: { thinking: { type: "adaptive" } },
  # Try these models if the primary fails with a retryable error:
  fallback: ["openai/gpt-5.5", "gemini/gemini-3-flash-preview"]
)

resp.message.tool_calls.each do |tc|
  puts "#{tc.name} -> #{tc.arguments}"
end
```

`tools` and `tool_choice` are folded into `provider_config` (passed straight
through to the provider). `max_tokens`, `temperature`, `top_p`, `top_k`,
`stop`, and `reasoning_effort` are folded into the request `config`.

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
