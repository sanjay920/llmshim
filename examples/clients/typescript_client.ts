/**
 * llmshim TypeScript/Bun client example.
 *
 * Usage:
 *   cargo run --features proxy --bin llmshim-proxy &
 *   bun run examples/clients/typescript_client.ts
 *   # or: npx tsx examples/clients/typescript_client.ts
 */

const BASE_URL = "http://localhost:3000";

// ============================================================
// Types (matches the OpenAPI spec)
// ============================================================

interface Message {
  role: "system" | "user" | "assistant" | "tool";
  content: string | null;
  tool_call_id?: string;
  tool_calls?: any[];
}

interface Config {
  max_tokens?: number;
  temperature?: number;
  top_p?: number;
  reasoning_effort?: "low" | "medium" | "high";
}

interface ChatRequest {
  model: string;
  messages: Message[];
  stream?: boolean;
  config?: Config;
  provider_config?: Record<string, any>;
}

interface ChatResponse {
  id: string;
  model: string;
  provider: string;
  message: { role: string; content: string | null; tool_calls?: any[] };
  reasoning?: string;
  usage: { input_tokens: number; output_tokens: number; reasoning_tokens?: number; total_tokens: number };
  latency_ms: number;
}

interface StreamEvent {
  type: "content" | "reasoning" | "tool_call" | "usage" | "done" | "error";
  text?: string;
  message?: string;
  input_tokens?: number;
  output_tokens?: number;
}

interface ModelEntry {
  id: string;
  provider: string;
  name: string;
}

// ============================================================
// Client functions
// ============================================================

async function chat(req: ChatRequest): Promise<ChatResponse> {
  const resp = await fetch(`${BASE_URL}/v1/chat`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(req),
  });
  if (!resp.ok) {
    const err = await resp.json();
    throw new Error(`${err.error.code}: ${err.error.message}`);
  }
  return resp.json();
}

async function* chatStream(req: ChatRequest): AsyncGenerator<{ event: string; data: StreamEvent }> {
  const resp = await fetch(`${BASE_URL}/v1/chat/stream`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(req),
  });
  if (!resp.ok) {
    const err = await resp.json();
    throw new Error(`${err.error.code}: ${err.error.message}`);
  }

  const reader = resp.body!.getReader();
  const decoder = new TextDecoder();
  let buffer = "";
  let currentEvent = "";

  while (true) {
    const { done, value } = await reader.read();
    if (done) break;

    buffer += decoder.decode(value, { stream: true });
    const lines = buffer.split("\n");
    buffer = lines.pop()!;

    for (const line of lines) {
      if (line.startsWith("event: ")) {
        currentEvent = line.slice(7);
      } else if (line.startsWith("data: ")) {
        const data = JSON.parse(line.slice(6));
        yield { event: currentEvent, data };
      }
    }
  }
}

async function listModels(): Promise<ModelEntry[]> {
  const resp = await fetch(`${BASE_URL}/v1/models`);
  const data = await resp.json();
  return data.models;
}

// ============================================================
// Demo
// ============================================================

async function main() {
  // List models
  console.log("Available models:");
  const models = await listModels();
  for (const m of models) {
    console.log(`  ${m.id}`);
  }
  console.log();

  // Non-streaming
  console.log("=== Non-streaming (Claude Sonnet 4.6) ===");
  const result = await chat({
    model: "anthropic/claude-sonnet-4-6",
    messages: [{ role: "user", content: "What is Rust? One sentence." }],
    config: { max_tokens: 200 },
  });
  console.log(`  Provider: ${result.provider}`);
  console.log(`  Content: ${result.message.content}`);
  console.log(`  Tokens: ↑${result.usage.input_tokens} ↓${result.usage.output_tokens}`);
  console.log(`  Latency: ${result.latency_ms}ms`);
  console.log();

  // Streaming
  console.log("=== Streaming (Gemini Flash) ===");
  for await (const { event, data } of chatStream({
    model: "gemini/gemini-3-flash-preview",
    messages: [{ role: "user", content: "Write a haiku about TypeScript." }],
    config: { max_tokens: 200 },
  })) {
    if (event === "content") {
      process.stdout.write(data.text!);
    } else if (event === "usage") {
      console.log(`\n  [↑${data.input_tokens} ↓${data.output_tokens}]`);
    } else if (event === "done") {
      console.log();
    }
  }

  // Multi-model conversation
  console.log("=== Multi-model conversation ===");
  const messages: Message[] = [{ role: "user", content: "Name a color. Just one word." }];

  const r1 = await chat({ model: "anthropic/claude-sonnet-4-6", messages, config: { max_tokens: 100 } });
  console.log(`  Claude: ${r1.message.content}`);
  messages.push({ role: "assistant", content: r1.message.content });

  messages.push({ role: "user", content: "Name a fruit that color. Just one word." });
  const r2 = await chat({ model: "openai/gpt-5.4", messages, config: { max_tokens: 100 } });
  console.log(`  GPT:    ${r2.message.content}`);
}

main().catch(console.error);
