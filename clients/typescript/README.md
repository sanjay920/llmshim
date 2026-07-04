# @llmshim/client

Thin, **dependency-free** TypeScript/JavaScript client for the [llmshim](https://crates.io/crates/llmshim) proxy.

It is pure HTTP: it talks to a **running** llmshim proxy over the network (default `http://localhost:3000`). Unlike the Python package, it does not bundle or spawn the Rust binary — start the proxy yourself:

```bash
llmshim proxy            # requires a build with --features proxy
```

- Zero runtime dependencies (built-in `fetch` + `ReadableStream`, Node 18+).
- Faithful to the proxy's OpenAPI contract (`api/openapi.yaml`).
- Typed responses and a typed `StreamEvent` union.

## Install

```bash
npm install @llmshim/client
```

## 30-second quickstart

```ts
import { Client } from "@llmshim/client";

const client = new Client(); // defaults to http://localhost:3000

// Non-streaming
const res = await client.chat({
  model: "anthropic/claude-sonnet-4-6",
  messages: [{ role: "user", content: "What is Rust in one sentence?" }],
});
console.log(res.message.content);

// Streaming — an async iterator of typed events
for await (const ev of client.stream({
  model: "gpt-5.5",
  messages: [{ role: "user", content: "Write a haiku about the ocean." }],
})) {
  if (ev.type === "reasoning") process.stdout.write(`\x1b[2m${ev.text}\x1b[0m`);
  if (ev.type === "content") process.stdout.write(ev.text);
  if (ev.type === "error") throw new Error(ev.message);
}

// Discovery
console.log(await client.models());
console.log(await client.health());
```

## Configuration

```ts
new Client({
  baseUrl: "http://localhost:3000", // proxy URL
  headers: { authorization: "Bearer …" }, // sent on every request
  fetch: myFetch, // custom fetch (optional; defaults to global fetch)
});
```

`createClient(options)` is also exported as an equivalent factory.

## Errors

Non-2xx responses throw a typed `LlmshimError` carrying `status`, `code`, and `message`
(parsed from the proxy's `{ error: { code, message } }` envelope):

```ts
import { LlmshimError } from "@llmshim/client";

try {
  await client.chat({ model: "", messages: [] });
} catch (err) {
  if (err instanceof LlmshimError) {
    console.error(err.status, err.code, err.message);
  }
}
```

## API

| Method                | Endpoint               | Returns                              |
| --------------------- | ---------------------- | ------------------------------------ |
| `client.chat(req)`    | `POST /v1/chat`        | `Promise<ChatResponse>`              |
| `client.stream(req)`  | `POST /v1/chat/stream` | `AsyncGenerator<StreamEvent>`        |
| `client.models()`     | `GET /v1/models`       | `Promise<ModelsResponse>`            |
| `client.health()`     | `GET /health`          | `Promise<HealthResponse>`            |

All request/response schemas are exported as TypeScript types (`ChatRequest`,
`Message`, `Config`, `ChatResponse`, `ResponseMessage`, `ToolCall`, `Usage`,
`StreamEvent`, `ModelsResponse`, `HealthResponse`, `ErrorResponse`, …).

## Develop

```bash
npm install
npm run build   # tsc → dist/
npm test        # node --test, fully mocked (no network, no API cost)
```
