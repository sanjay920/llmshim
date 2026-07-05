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

## Configuration

```ts
new Client({
  baseUrl: "http://localhost:3000", // connect to a proxy you're already running — disables auto-spawn
  headers: { authorization: "Bearer …" }, // sent on every request
  fetch: myFetch, // custom fetch (optional; defaults to global fetch)
});
```

Omit `baseUrl` to auto-spawn the bundled binary instead (see below). `createClient(options)` is also exported as an equivalent factory — both are fully synchronous; the auto-spawn happens lazily inside the first `chat`/`stream`/`models`/`health` call, not at construction time.

## How the bundled binary works

`npm install llmshim` also installs one of five tiny `optionalDependencies` — `llmshim-darwin-arm64`, `llmshim-darwin-x64`, `llmshim-linux-x64`, `llmshim-linux-arm64`, or `llmshim-win32-x64` — npm resolves the one matching your platform automatically via each package's `os`/`cpu` fields, so only ~8MB for your actual platform ever downloads. Each contains nothing but the prebuilt `llmshim` binary.

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
