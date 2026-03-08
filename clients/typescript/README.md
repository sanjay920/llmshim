# llmshim TypeScript Client

Generated from the llmshim OpenAPI spec. Talk to any LLM through one API.

## Install

```bash
npm install openapi-fetch
# Types are included — no extra @types package needed
```

## Quick Start

```typescript
import { LlmShim } from "./src/index.ts";

const client = new LlmShim(); // defaults to http://localhost:3000

// Simple chat — pass a string for a single user message
const resp = await client.chat("claude-sonnet-4-6", "What is Rust?");
console.log(resp.message.content);

// With config
const resp2 = await client.chat(
  "openai/gpt-5.4",
  "Explain quicksort",
  { max_tokens: 500, temperature: 0.7 }
);
```

## Streaming

```typescript
for await (const chunk of client.stream("claude-sonnet-4-6", "Write a poem")) {
  if (chunk.type === "content") {
    process.stdout.write(chunk.text!);
  } else if (chunk.type === "reasoning") {
    // thinking tokens — show or skip
  } else if (chunk.type === "usage") {
    console.log(`\n[${chunk.input_tokens}in / ${chunk.output_tokens}out]`);
  }
}
```

## Multi-Model Conversations

Switch models mid-conversation. History carries over.

```typescript
import type { Message } from "./src/index.ts";

const messages: Message[] = [
  { role: "system", content: "You are a helpful tutor." },
  { role: "user", content: "What is a closure?" },
];

// Ask Claude
const r1 = await client.chat("claude-sonnet-4-6", messages, { max_tokens: 500 });
console.log(`Claude: ${r1.message.content}`);

// Continue with GPT
messages.push({ role: "assistant", content: r1.message.content as string });
messages.push({ role: "user", content: "Now explain it differently." });
const r2 = await client.chat("gpt-5.4", messages, { max_tokens: 500 });
console.log(`GPT: ${r2.message.content}`);
```

## Reasoning / Thinking

```typescript
// Via config (works across all providers)
const resp = await client.chat(
  "claude-sonnet-4-6",
  "What is 15*37?",
  { max_tokens: 4000, reasoning_effort: "high" }
);
console.log(resp.reasoning); // thinking content
console.log(resp.message.content); // answer

// Via provider-specific config
const resp2 = await client.chat(
  "claude-sonnet-4-6",
  "Solve: x^2 - 5x + 6 = 0",
  { max_tokens: 4000 },
  { thinking: { type: "enabled", budget_tokens: 2000 } }
);
```

## Other Methods

```typescript
// List available models
const models = await client.models();
models.forEach(m => console.log(`${m.id} (${m.provider})`));

// Health check
const health = await client.health();
console.log(health); // { status: "ok", providers: ["openai", "anthropic", ...] }
```

## Types

All types are auto-generated from the OpenAPI spec:

```typescript
import type {
  ChatRequest,
  ChatResponse,
  Message,
  Config,
  Usage,
  StreamChunk,
  ModelEntry,
} from "./src/index.ts";
```

## Running the Server

```bash
# From the llmshim repo root
cargo run --features proxy --bin llmshim-proxy
```

Set API keys in `.env` or as environment variables: `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `GEMINI_API_KEY`, `XAI_API_KEY`.
