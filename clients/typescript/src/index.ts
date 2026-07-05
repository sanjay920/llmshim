/**
 * llmshim TypeScript client.
 *
 * A dependency-free HTTP client for the llmshim proxy. If you don't pass a
 * `baseUrl`, it bundles and auto-starts a prebuilt proxy binary for your
 * platform (like the Python client) — nothing to run yourself. Pass an
 * explicit `baseUrl` to talk to a proxy you're already running instead.
 *
 * @example
 * import { Client } from "llmshim";
 * const client = new Client(); // auto-starts the bundled proxy on first request
 * const res = await client.chat({
 *   model: "anthropic/claude-sonnet-4-6",
 *   messages: [{ role: "user", content: "Hello!" }],
 * });
 * console.log(res.message.content);
 */

export * from "./types.js";
export { ensureServer, platformPackageName } from "./server.js";

import type {
  ChatRequest,
  ChatResponse,
  ErrorResponse,
  HealthResponse,
  ModelsResponse,
  StreamEvent,
} from "./types.js";
import { ensureServer } from "./server.js";

/** A `fetch` implementation. Defaults to the global `fetch` (Node 18+). */
export type FetchLike = typeof fetch;

/** Options for constructing a {@link Client}. */
export interface ClientOptions {
  /**
   * Base URL of a proxy you're already running. If omitted, the client
   * bundles and auto-starts a prebuilt proxy binary on the first request
   * (see {@link ensureServer}) instead of assuming a fixed default.
   */
  baseUrl?: string;
  /** Custom fetch implementation. Defaults to the global `fetch`. */
  fetch?: FetchLike;
  /** Extra headers sent with every request. */
  headers?: Record<string, string>;
}

/**
 * Error thrown when the proxy returns a non-2xx response.
 * Carries the HTTP status plus the `code`/`message` from the ErrorResponse body.
 */
export class LlmshimError extends Error {
  /** HTTP status code. */
  readonly status: number;
  /** Machine-readable error code from the proxy (empty if unavailable). */
  readonly code: string;

  constructor(status: number, code: string, message: string) {
    super(message);
    this.name = "LlmshimError";
    this.status = status;
    this.code = code;
  }
}

/** HTTP client for the llmshim proxy. */
export class Client {
  /** Set only when an explicit `baseUrl` was passed at construction. */
  private readonly explicitBaseUrl: string | undefined;
  /** Memoized auto-start resolution, shared across calls on this instance. */
  private autoBaseUrl: Promise<string> | undefined;
  private readonly fetchImpl: FetchLike;
  private readonly headers: Record<string, string>;

  constructor(options: ClientOptions = {}) {
    this.explicitBaseUrl = options.baseUrl?.replace(/\/+$/, "");
    const f = options.fetch ?? globalThis.fetch;
    if (typeof f !== "function") {
      throw new Error(
        "No fetch implementation available. Use Node 18+ or pass `fetch` in ClientOptions.",
      );
    }
    // Bind to preserve `this` for the global fetch.
    this.fetchImpl = f === globalThis.fetch ? f.bind(globalThis) : f;
    this.headers = { ...options.headers };
  }

  /**
   * Resolve the base URL to use for the next request: the explicit `baseUrl`
   * if one was given at construction, otherwise the bundled proxy's URL
   * (starting it on first call). Memoized so the bundled proxy is only
   * started once per `Client` instance.
   */
  private resolveBaseUrl(): Promise<string> {
    if (this.explicitBaseUrl) return Promise.resolve(this.explicitBaseUrl);
    if (!this.autoBaseUrl) this.autoBaseUrl = ensureServer();
    return this.autoBaseUrl;
  }

  /**
   * Send a chat completion request to POST /v1/chat.
   * Non-streaming by default; if `req.stream` is true the proxy streams instead,
   * so prefer {@link Client.stream} for streaming.
   */
  async chat(req: ChatRequest): Promise<ChatResponse> {
    const res = await this.post("/v1/chat", req);
    await throwIfError(res);
    return (await res.json()) as ChatResponse;
  }

  /**
   * Send a streaming chat request to POST /v1/chat/stream.
   * Returns an async iterator of typed {@link StreamEvent}s.
   *
   * @example
   * for await (const ev of client.stream({ model, messages })) {
   *   if (ev.type === "content") process.stdout.write(ev.text);
   * }
   */
  async *stream(req: ChatRequest): AsyncGenerator<StreamEvent, void, unknown> {
    const res = await this.post("/v1/chat/stream", { ...req, stream: true });
    await throwIfError(res);
    if (!res.body) {
      throw new LlmshimError(res.status, "no_body", "Streaming response had no body");
    }
    yield* parseSse(res.body);
  }

  /** List available models via GET /v1/models. */
  async models(): Promise<ModelsResponse> {
    const res = await this.get("/v1/models");
    await throwIfError(res);
    return (await res.json()) as ModelsResponse;
  }

  /** Health check via GET /health. */
  async health(): Promise<HealthResponse> {
    const res = await this.get("/health");
    await throwIfError(res);
    return (await res.json()) as HealthResponse;
  }

  private async post(path: string, body: unknown): Promise<Response> {
    const baseUrl = await this.resolveBaseUrl();
    return this.fetchImpl(baseUrl + path, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        accept: "application/json, text/event-stream",
        ...this.headers,
      },
      body: JSON.stringify(body),
    });
  }

  private async get(path: string): Promise<Response> {
    const baseUrl = await this.resolveBaseUrl();
    return this.fetchImpl(baseUrl + path, {
      method: "GET",
      headers: { accept: "application/json", ...this.headers },
    });
  }
}

/** Convenience factory mirroring `new Client(options)`. */
export function createClient(options?: ClientOptions): Client {
  return new Client(options);
}

/** Throw a {@link LlmshimError} for non-2xx responses, parsing ErrorResponse when present. */
async function throwIfError(res: Response): Promise<void> {
  if (res.ok) return;
  let code = "";
  let message = `HTTP ${res.status}`;
  const text = await res.text().catch(() => "");
  if (text) {
    try {
      const parsed = JSON.parse(text) as Partial<ErrorResponse>;
      if (parsed.error) {
        code = parsed.error.code ?? "";
        message = parsed.error.message ?? message;
      } else {
        message = text;
      }
    } catch {
      message = text;
    }
  }
  throw new LlmshimError(res.status, code, message);
}

/**
 * Parse an SSE byte stream into typed {@link StreamEvent}s.
 *
 * Handles CRLF/LF line endings, multi-line `data:` fields (joined with "\n"),
 * comment lines, and `[DONE]` termination. The `type` discriminant is taken
 * from the SSE `event:` field, falling back to a `type` key inside the JSON.
 */
export async function* parseSse(
  body: ReadableStream<Uint8Array>,
): AsyncGenerator<StreamEvent, void, unknown> {
  const decoder = new TextDecoder();
  const reader = body.getReader();
  let buffer = "";

  try {
    while (true) {
      const { done, value } = await reader.read();
      if (value) buffer += decoder.decode(value, { stream: true });
      if (done) {
        buffer += decoder.decode();
      }

      // SSE events are separated by a blank line. Support both \n\n and \r\n\r\n.
      let sep: number;
      while ((sep = indexOfEventBoundary(buffer)) !== -1) {
        const rawEvent = buffer.slice(0, sep);
        buffer = buffer.slice(sep + boundaryLength(buffer, sep));
        const parsed = parseEventBlock(rawEvent);
        if (parsed) yield parsed;
      }

      if (done) break;
    }

    // Flush any trailing event that wasn't terminated by a blank line.
    const parsed = parseEventBlock(buffer);
    if (parsed) yield parsed;
  } finally {
    reader.releaseLock();
  }
}

/** Find the index of the next blank-line event boundary, or -1. */
function indexOfEventBoundary(buf: string): number {
  const lf = buf.indexOf("\n\n");
  const crlf = buf.indexOf("\r\n\r\n");
  if (lf === -1) return crlf;
  if (crlf === -1) return lf;
  return Math.min(lf, crlf);
}

/** Length of the boundary sequence at position `sep`. */
function boundaryLength(buf: string, sep: number): number {
  return buf.startsWith("\r\n\r\n", sep) ? 4 : 2;
}

/** Parse a single SSE event block into a StreamEvent, or null if it carries no data. */
function parseEventBlock(block: string): StreamEvent | null {
  let eventType = "";
  const dataLines: string[] = [];

  for (const rawLine of block.split(/\r\n|\n|\r/)) {
    const line = rawLine;
    if (line === "" || line.startsWith(":")) continue; // blank or comment
    const colon = line.indexOf(":");
    const field = colon === -1 ? line : line.slice(0, colon);
    // Per SSE spec, a single leading space after the colon is stripped.
    let val = colon === -1 ? "" : line.slice(colon + 1);
    if (val.startsWith(" ")) val = val.slice(1);

    if (field === "event") eventType = val;
    else if (field === "data") dataLines.push(val);
  }

  if (dataLines.length === 0) return null;
  const data = dataLines.join("\n");
  if (data === "[DONE]") return { type: "done" };

  let payload: Record<string, unknown>;
  try {
    payload = JSON.parse(data) as Record<string, unknown>;
  } catch {
    return null;
  }

  const type = eventType || (typeof payload.type === "string" ? payload.type : "");
  return { ...payload, type } as StreamEvent;
}
