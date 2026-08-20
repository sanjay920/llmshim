# llmshim Go client

A thin, idiomatic Go client for the [llmshim](https://crates.io/crates/llmshim)
proxy. It speaks pure HTTP to a running llmshim proxy — it does **not** spawn
the Rust binary and has **no third-party dependencies** (standard library only).

Module path: `github.com/sanjay920/llmshim/clients/go`

## Prerequisites

Start an llmshim proxy (built with the `proxy` feature):

```bash
llmshim proxy   # listens on :3000 by default
```

## Install

```bash
go get github.com/sanjay920/llmshim/clients/go
```

## Quickstart

```go
package main

import (
	"context"
	"fmt"
	"log"

	llmshim "github.com/sanjay920/llmshim/clients/go"
)

func main() {
	ctx := context.Background()
	client := llmshim.New() // defaults to http://localhost:3000

	resp, err := client.Chat(ctx, llmshim.ChatRequest{
		Model:    "anthropic/claude-sonnet-4-6",
		Messages: []llmshim.Message{{Role: "user", Content: "What is Rust?"}},
	})
	if err != nil {
		log.Fatal(err)
	}
	fmt.Println(resp.Message.Content)
}
```

## Streaming

`Stream` returns a channel of typed events. It is closed when the stream ends
(on a `done` event or EOF). Client-side transport/parse errors arrive as an
event with `Err` set.

```go
ch, err := client.Stream(ctx, llmshim.ChatRequest{
	Model:    "anthropic/claude-sonnet-4-6",
	Messages: []llmshim.Message{{Role: "user", Content: "Write a haiku"}},
})
if err != nil {
	log.Fatal(err)
}
for ev := range ch {
	switch ev.Type {
	case llmshim.EventContent:
		fmt.Print(ev.Text)
	case llmshim.EventReasoning:
		fmt.Print(ev.Text) // model thinking
	case llmshim.EventToolCall:
		fmt.Printf("\n[tool %s(%s)]\n", ev.Name, ev.Arguments)
	case llmshim.EventUsage:
		fmt.Printf("\n[tokens: %d in / %d out]\n", ev.InputTokens, ev.OutputTokens)
	case llmshim.EventError:
		log.Fatal(ev.Message)
	}
	if ev.Err != nil {
		log.Fatal(ev.Err)
	}
}
```

## Config and provider passthrough

```go
maxTokens := 1024
temp := 0.7
resp, err := client.Chat(ctx, llmshim.ChatRequest{
	Model:    "openai/gpt-5.6-sol",
	Messages: []llmshim.Message{{Role: "user", Content: "Hi"}},
	Config: &llmshim.Config{
		MaxTokens:   &maxTokens,
		Temperature: &temp,
		// "none" | "low" | "medium" | "high" | "xhigh" | "max".
		// llmshim clamps to the nearest tier the target model supports.
		ReasoningEffort: "high",
		// "standard" (default) or "pro". "pro" requests substantially more
		// model work: native on OpenAI gpt-5.6/-pro, emulated as a one-tier
		// effort bump elsewhere.
		ReasoningMode: "pro",
	},
	// Ordered fallback models tried on retryable errors (429/500/502/503).
	Fallback: []string{"gemini/gemini-3.5-flash"},
})
```

### How `ProviderConfig` (`provider_config`) works

`ProviderConfig` is merged into the underlying request **at the request root**,
not nested under any wrapper. So provider-native controls **must** be namespaced
under an `x-<provider>` key — a bare key (e.g. a top-level `thinking`) lands at
the root where no provider reads it and is silently ignored.

Each provider's transform pulls the settings out of its own `x-<provider>`
namespace and drops the namespace before the request goes upstream. Use
`x-anthropic`, `x-gemini`, `x-openai`, `x-xai`, `x-openrouter`, `x-vllm`, or
`x-sglang`. `provider_config` also carries the provider-agnostic keys `tools`,
`response_format`, and `reasoning_summary` at the root.

```go
resp, err := client.Chat(ctx, llmshim.ChatRequest{
	Model:    "anthropic/claude-sonnet-5",
	Messages: []llmshim.Message{{Role: "user", Content: "Prove there are infinitely many primes."}},
	// Namespaced under x-anthropic so Anthropic's transform picks it up.
	ProviderConfig: map[string]any{
		"x-anthropic": map[string]any{
			"thinking": map[string]any{
				"type":          "enabled",
				"budget_tokens": 4000,
			},
		},
	},
})
```

## Tools (function calling)

Send tool definitions through `ProviderConfig["tools"]` (OpenAI Chat
Completions format — llmshim translates them to each provider's native shape).
Tool calls come back on `resp.Message.ToolCalls` (non-streaming) or as
`tool_call` stream events. Reply with a `role: "tool"` message that echoes the
`tool_call_id` to round-trip the result.

```go
tools := []map[string]any{{
	"type": "function",
	"function": map[string]any{
		"name":        "get_weather",
		"description": "Get the current weather for a city.",
		"parameters": map[string]any{
			"type": "object",
			"properties": map[string]any{
				"city": map[string]any{"type": "string"},
			},
			"required": []string{"city"},
		},
	},
}}

messages := []llmshim.Message{{Role: "user", Content: "What's the weather in Paris?"}}

resp, err := client.Chat(ctx, llmshim.ChatRequest{
	Model:          "openai/gpt-5.6-sol",
	Messages:       messages,
	ProviderConfig: map[string]any{"tools": tools},
})
if err != nil {
	log.Fatal(err)
}

// Read the tool calls the model wants to make.
for _, tc := range resp.Message.ToolCalls {
	fmt.Printf("call %s: %s(%s)\n", tc.ID, tc.Function.Name, tc.Function.Arguments)
}

// Append the assistant turn, then a tool result keyed by ToolCallID, and
// call again to let the model use the result.
messages = append(messages,
	llmshim.Message{Role: "assistant", ToolCalls: resp.Message.ToolCalls},
	llmshim.Message{
		Role:       "tool",
		ToolCallID: resp.Message.ToolCalls[0].ID,
		Content:    `{"temp_c": 21, "conditions": "sunny"}`,
	},
)

final, err := client.Chat(ctx, llmshim.ChatRequest{
	Model:          "openai/gpt-5.6-sol",
	Messages:       messages,
	ProviderConfig: map[string]any{"tools": tools},
})
if err != nil {
	log.Fatal(err)
}
fmt.Println(final.Message.Content)
```

When streaming, tool calls arrive as `EventToolCall` events (`ev.ID`, `ev.Name`,
`ev.Arguments`):

```go
for ev := range ch {
	if ev.Type == llmshim.EventToolCall {
		fmt.Printf("[tool %s: %s(%s)]\n", ev.ID, ev.Name, ev.Arguments)
	}
}
```

## Providers

Select a provider with the `provider/model` prefix on `Model` (or let llmshim
auto-detect from the model name). Each provider reads its API key / base URL
from environment variables, and provider-native controls go under its
`x-<provider>` namespace in `ProviderConfig` (see above).

| Provider | Model string | Environment | Namespace |
| --- | --- | --- | --- |
| OpenAI | `openai/gpt-5.6-sol` | `OPENAI_API_KEY` | `x-openai` |
| Anthropic | `anthropic/claude-sonnet-5` | `ANTHROPIC_API_KEY` | `x-anthropic` |
| Gemini | `gemini/gemini-3.5-flash` | `GEMINI_API_KEY` | `x-gemini` |
| xAI | `xai/grok-4.5` | `XAI_API_KEY` | `x-xai` |
| OpenRouter | `openrouter/<vendor>/<model>` | `OPENROUTER_API_KEY` | `x-openrouter` |
| vLLM | `vllm/<served-model>` | `VLLM_BASE_URL` (+ optional `VLLM_API_KEY`) | `x-vllm` |
| SGLang | `sglang/<served-model>` | `SGLANG_BASE_URL` (+ optional `SGLANG_API_KEY`) | `x-sglang` |

**OpenRouter** routes to any upstream vendor, e.g.
`openrouter/anthropic/claude-sonnet-4.5`. Steer routing with `x-openrouter`
controls such as `provider` (e.g. `{"sort": "throughput"}`), `models`, and
`transforms`:

```go
ProviderConfig: map[string]any{
	"x-openrouter": map[string]any{
		"provider": map[string]any{"sort": "throughput"},
	},
}
```

**vLLM / SGLang** target your own self-hosted (local or remote) OpenAI-compatible
server. Point the base-URL env var at it and use the served model name:

```go
// VLLM_BASE_URL=http://localhost:8000/v1
resp, err := client.Chat(ctx, llmshim.ChatRequest{
	Model:    "vllm/meta-llama/Llama-3.1-8B-Instruct",
	Messages: []llmshim.Message{{Role: "user", Content: "Hi"}},
	ProviderConfig: map[string]any{
		"x-vllm": map[string]any{"top_k": 40},
	},
})
```

## Other endpoints

```go
models, _ := client.Models(ctx)   // GET /v1/models
health, _ := client.Health(ctx)   // GET /health
```

## Options

```go
client := llmshim.New(
	llmshim.WithBaseURL("http://localhost:8080"),
	llmshim.WithHTTPClient(&http.Client{Timeout: 30 * time.Second}),
	llmshim.WithHeader("Authorization", "Bearer ..."),
)
```

## Errors

Non-2xx responses are returned as `*llmshim.APIError` with `StatusCode`, `Code`,
and `Message`:

```go
_, err := client.Chat(ctx, req)
var apiErr *llmshim.APIError
if errors.As(err, &apiErr) {
	fmt.Println(apiErr.StatusCode, apiErr.Code, apiErr.Message)
}
```

## Testing

Tests use `net/http/httptest` with canned JSON and SSE responses — they never
call real provider APIs or require a running proxy:

```bash
go test ./...
```
