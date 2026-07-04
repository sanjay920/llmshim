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
	Model:    "openai/gpt-5.5",
	Messages: []llmshim.Message{{Role: "user", Content: "Hi"}},
	Config: &llmshim.Config{
		MaxTokens:       &maxTokens,
		Temperature:     &temp,
		ReasoningEffort: "high", // "low" | "medium" | "high"
	},
	// Raw provider-specific JSON (Anthropic thinking, Gemini safety, tools, ...)
	ProviderConfig: map[string]any{
		"thinking": map[string]any{"type": "adaptive"},
	},
	// Ordered fallback models tried on retryable errors (429/500/502/503).
	Fallback: []string{"gemini/gemini-3-flash-preview"},
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
