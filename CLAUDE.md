# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What is llmshim

A pure Rust LLM API translation layer. Takes OpenAI-format JSON requests, translates them to provider-native formats (and back), with zero infrastructure requirements. Supports OpenAI (Responses API), Anthropic, Google Gemini, and xAI. Includes an interactive CLI chat with streaming, reasoning, and mid-conversation model switching.

## Supported models

- **OpenAI:** `gpt-5.4`
- **Anthropic:** `claude-opus-4-6`, `claude-sonnet-4-6`, `claude-haiku-4-5-20251001`
- **Gemini:** `gemini-3.1-pro-preview`, `gemini-3-flash-preview`, `gemini-3.1-flash-lite-preview`
- **xAI:** `grok-4-1-fast-reasoning`, `grok-4-1-fast-non-reasoning`

## Build & Test

```bash
cargo build                                          # dev build
cargo build --release                                # release build (~3.7MB binary)
cargo test --tests                                   # unit tests (~168)
cargo test -- --ignored                              # integration tests (needs API keys)
cargo run                                            # interactive CLI chat
```

API keys: `.env` in project root (auto-loaded by CLI) or env vars `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY`, `XAI_API_KEY`.

## Architecture

### Value-based transforms, no canonical struct

Requests flow as `serde_json::Value`. Each provider's transform takes raw JSON and maps only what it understands. Provider-specific features use `x-anthropic`, `x-gemini` namespaces.

### Request flow

```
llmshim::completion(router, request)
  → router.resolve("anthropic/claude-sonnet-4-6")   // parse "provider/model"
  → provider.transform_request(model, &value)        // OpenAI JSON → provider-native
  → client.send(provider_request)                    // HTTP
  → provider.transform_response(model, body)         // provider-native → OpenAI JSON
```

### Provider trait (`src/provider.rs`)

Every provider implements: `transform_request`, `transform_response`, `transform_stream_chunk`.

### Router (`src/router.rs`)

Parses `"provider/model"` strings. Auto-infers provider from prefix (`gpt*`/`o*` → openai, `claude*` → anthropic, `gemini*` → gemini, `grok*` → xai). Supports aliases. `Router::from_env()` reads API key env vars.

### Streaming (`src/client.rs`)

`SseStream` buffers bytes, extracts `data:` lines, routes through provider's `transform_stream_chunk`. Returns `None` to skip non-content events.

### Multi-model conversations

Each provider sanitizes messages from other providers in `transform_request`. OpenAI's `annotations`/`refusal` stripped for Anthropic/Gemini. `reasoning_content` stripped for all. Tool calls normalized to OpenAI format in responses, translated back per-provider on input.

### CLI (`src/main.rs`)

Interactive chat with streaming. `/model` to switch, `/clear` to reset. Reasoning on by default (`reasoning_effort: "high"`). Thinking tokens shown in dim grey, answers in default color. Final summary shows timing and token counts (`↑` input, `↓` output). Optional JSONL file logging via `--log <path>` or `LLMSHIM_LOG` env var.

### Logging (`src/log.rs`)

JSONL structured logging. Each entry: timestamp, model, provider, latency_ms, input/output/reasoning token counts, status, request_id. Logged from API-reported usage (not local counting). CLI shows summary after each response; file logging is opt-in.

## Detailed reference

Provider API format details are in `.claude/rules/provider-api-formats.md`. Testing conventions and gotchas are in `.claude/rules/testing.md`. These load automatically when working in relevant files.
