# llmshim

**Zero-runtime-dependency** TypeScript/JavaScript client for the [llmshim](https://crates.io/crates/llmshim) proxy.

Like the Python package, it bundles a prebuilt proxy binary for your platform and auto-starts it on the first request — nothing to run yourself. Pass an explicit `baseUrl` instead if you'd rather point it at a proxy you're already running (Docker, a shared server, `llmshim proxy` in another terminal).

- Bundles a prebuilt binary per platform (macOS/Linux/Windows, x64/arm64) via `optionalDependencies` — the same pattern esbuild/swc/Turborepo use. No native dependency, no postinstall scripts.
- Zero runtime dependencies otherwise (built-in `fetch` + `ReadableStream` + Node builtins for the auto-spawn, Node 18+).
- Faithful to the proxy's OpenAPI contract (`api/openapi.yaml`).
- Typed responses and a typed `StreamEvent` union.

## Install

```bash
npm install llmshim
```

## 30-second quickstart

```ts
import { Client } from "llmshim";

const client = new Client(); // no baseUrl -> auto-starts the bundled proxy on first request

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

## Providers

Pick a provider by prefixing the model with `provider/`. Auto-detection also
works for well-known names, but the explicit form is unambiguous. Each provider
reads its own credentials from the environment (or `llmshim configure`):

| Provider   | Model string                                       | Env vars                                          |
| ---------- | -------------------------------------------------- | ------------------------------------------------- |
| OpenAI     | `openai/gpt-5.6-sol`                               | `OPENAI_API_KEY`                                  |
| Anthropic  | `anthropic/claude-sonnet-5`                        | `ANTHROPIC_API_KEY`                               |
| Gemini     | `gemini/gemini-3.5-flash`                          | `GEMINI_API_KEY`                                  |
| xAI        | `xai/grok-4.5`                                     | `XAI_API_KEY`                                     |
| OpenRouter | `openrouter/anthropic/claude-sonnet-4.5`          | `OPENROUTER_API_KEY`                              |
| vLLM       | `vllm/<served-model>`                              | `VLLM_BASE_URL` (+ optional `VLLM_API_KEY`)       |
| SGLang     | `sglang/<served-model>`                            | `SGLANG_BASE_URL` (+ optional `SGLANG_API_KEY`)   |

`vllm`/`sglang` target any self-hosted (local or remote) OpenAI-compatible
server — point the base URL at it and reference the served model name.

## Request configuration

Every request accepts three optional knobs alongside `model` and `messages`:

### `config` — provider-agnostic settings

llmshim maps these to each provider's native dialect (clamping where a model
doesn't support a value):

```ts
const res = await client.chat({
  model: "openai/gpt-5.6-sol",
  messages: [{ role: "user", content: "Explain quicksort." }],
  config: {
    reasoning_effort: "high", // none | low | medium | high | xhigh | max
    reasoning_mode: "standard", // standard | pro
    max_tokens: 1024,
    temperature: 0.7,
    top_p: 0.95,
    top_k: 40,
    stop: ["\n\n"],
  },
});
```

### `fallback` — try other models on retryable errors

An ordered list tried in turn on retryable upstream failures (429/5xx):

```ts
await client.chat({
  model: "anthropic/claude-sonnet-5",
  messages: [{ role: "user", content: "Hi" }],
  fallback: ["openai/gpt-5.6-sol", "gemini/gemini-3.5-flash"],
});
```

### `provider_config` — raw, provider-native passthrough

Merged into the underlying request at its **root**. Native controls must be
**namespaced** by provider (`x-anthropic`, `x-openai`, `x-gemini`,
`x-openrouter`, `x-vllm`, `x-sglang`); `tools`, `response_format`, and
`reasoning_summary` sit at the top level. Use it for anything `config` doesn't
cover:

```ts
await client.chat({
  model: "anthropic/claude-sonnet-5",
  messages: [{ role: "user", content: "Think hard about this." }],
  provider_config: {
    "x-anthropic": { thinking: { type: "enabled", budget_tokens: 4000 } },
  },
});

// OpenRouter routing preferences, for example:
await client.chat({
  model: "openrouter/anthropic/claude-sonnet-4.5",
  messages: [{ role: "user", content: "Hi" }],
  provider_config: {
    "x-openrouter": { provider: { sort: "throughput" } }, // also: models, transforms
  },
});
```

## Tools

Send tool definitions through `provider_config.tools` (OpenAI Chat Completions
format — llmshim translates to each provider's native shape). Read tool calls
from `message.tool_calls` on a non-streaming response, or from `tool_call`
events while streaming:

```ts
const tools = [
  {
    type: "function",
    function: {
      name: "get_weather",
      description: "Get the current weather for a city.",
      parameters: {
        type: "object",
        properties: { city: { type: "string" } },
        required: ["city"],
      },
    },
  },
];

// Non-streaming
const res = await client.chat({
  model: "openai/gpt-5.6-sol",
  messages: [{ role: "user", content: "What's the weather in Paris?" }],
  provider_config: { tools },
});
for (const call of res.message.tool_calls ?? []) {
  console.log(call.function?.name, call.function?.arguments); // "get_weather", '{"city":"Paris"}'
}

// Streaming
for await (const ev of client.stream({
  model: "openai/gpt-5.6-sol",
  messages: [{ role: "user", content: "What's the weather in Paris?" }],
  provider_config: { tools },
})) {
  if (ev.type === "tool_call") console.log(ev.name, ev.arguments);
}
```

## Reasoning

Request reasoning with `config.reasoning_effort` / `config.reasoning_mode`
(above). On a non-streaming response the model's reasoning text is on
`res.reasoning`; while streaming it arrives as `reasoning` events. To replay a
model's reasoning back on a later turn, set `reasoning_content` on the assistant
message you send:

```ts
messages.push({
  role: "assistant",
  content: res.message.content,
  reasoning_content: res.reasoning ?? undefined,
});
```

## Client options

```ts
new Client({
  baseUrl: "http://localhost:3000", // connect to a proxy you're already running — disables auto-spawn
  headers: { authorization: "Bearer …" }, // sent on every request
  fetch: myFetch, // custom fetch (optional; defaults to global fetch)
});
```

Omit `baseUrl` to auto-spawn the bundled binary instead (see below). `createClient(options)` is also exported as an equivalent factory — both are fully synchronous; the auto-spawn happens lazily inside the first `chat`/`stream`/`models`/`health` call, not at construction time.

## How the bundled binary works

`npm install llmshim` also installs one of five tiny `optionalDependencies` — `llmshim-darwin-arm64`, `llmshim-darwin-x64`, `llmshim-linux-x64`, `llmshim-linux-arm64`, or `@sanjay920/llmshim-win32-x64` (the Windows one is scoped to sidestep npm's spam filter on unscoped `*-win32-*` names) — npm resolves the one matching your platform automatically via each package's `os`/`cpu` fields, so only ~8MB for your actual platform ever downloads. Each contains nothing but the prebuilt `llmshim` binary.

On the first `chat`/`stream`/`models`/`health` call on a `Client` constructed without `baseUrl`, the client finds that bundled binary (falling back to `PATH`, e.g. a `cargo install llmshim`), starts `llmshim proxy` on a free local port, waits for it to accept connections, and reuses that same process for every subsequent call in the same Node process — mirroring the Python client's `_server.py`. It's stopped automatically on process exit.

If no bundled binary is found and nothing is on `PATH`, the first request throws a clear error explaining how to install one or pass an explicit `baseUrl`.

## Errors

Non-2xx responses throw a typed `LlmshimError` carrying `status`, `code`, and `message`
(parsed from the proxy's `{ error: { code, message } }` envelope):

```ts
import { LlmshimError } from "llmshim";

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

`scripts/manual-smoke-check.mjs` is a manual (not automated) end-to-end check of the auto-spawn path against a real binary — see the comment at the top of that file for how to run it. It's excluded from `npm test`'s auto-discovery and from the published package.

## Publishing (maintainers)

Six packages publish together on every release, all via npm's OIDC trusted publishing (no tokens stored): the five platform binary packages (`clients/typescript/packages/*`) and this root package. Each needs a one-time Trusted Publisher configured at `npmjs.com/package/<name>/access` with:

- Repository owner: `sanjay920`, repository: `llmshim`, workflow filename: `release.yml`, environment: `release`.

See `.github/workflows/release.yml` (`npm-macos-binaries`, `npm-linux-x64-binary`, `npm-linux-arm64-binary`, `npm-windows-binary`, `npm` jobs) and `.claude/skills/release/SKILL.md`.
