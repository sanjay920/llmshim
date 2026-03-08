/**
 * Battle test for the llmshim TypeScript client.
 * Run: node --experimental-strip-types test.ts
 * (requires proxy running: cargo run --features proxy --bin llmshim-proxy)
 */

import { LlmShim } from "./src/index.ts";
import type { Message } from "./src/index.ts";

const client = new LlmShim("http://localhost:3000");

let passed = 0;
let failed = 0;

function assert(condition: boolean, msg: string) {
  if (condition) {
    passed++;
    console.log(`  ✓ ${msg}`);
  } else {
    failed++;
    console.log(`  ✗ ${msg}`);
  }
}

async function test(name: string, fn: () => Promise<void>) {
  console.log(`\n${name}`);
  try {
    await fn();
  } catch (e: any) {
    failed++;
    console.log(`  ✗ EXCEPTION: ${e.message}`);
  }
}

await test("Health check", async () => {
  const h = await client.health();
  assert(h.status === "ok", "status is ok");
  assert(h.providers.length > 0, `has ${h.providers.length} providers`);
});

await test("List models", async () => {
  const models = await client.models();
  assert(models.length > 0, `got ${models.length} models`);
  assert(models[0].id.includes("/"), "model id has provider prefix");
  assert(!!models[0].provider, "model has provider field");
  assert(!!models[0].name, "model has name field");
});

await test("Chat — simple string message", async () => {
  const resp = await client.chat(
    "anthropic/claude-sonnet-4-6",
    "Say 'pong'. Just that word.",
    { max_tokens: 100 }
  );
  assert(resp.provider === "anthropic", `provider: ${resp.provider}`);
  assert(resp.message.role === "assistant", "role is assistant");
  assert(
    typeof resp.message.content === "string" &&
      resp.message.content.toLowerCase().includes("pong"),
    `content contains pong: ${resp.message.content}`
  );
  assert(resp.usage.input_tokens > 0, `input_tokens: ${resp.usage.input_tokens}`);
  assert(resp.latency_ms > 0, `latency: ${resp.latency_ms}ms`);
});

await test("Chat — message array with system", async () => {
  const resp = await client.chat(
    "anthropic/claude-sonnet-4-6",
    [
      { role: "system", content: "Always respond in exactly one word." },
      { role: "user", content: "What color is the sky?" },
    ],
    { max_tokens: 100 }
  );
  const words = (resp.message.content as string).trim().split(/\s+/);
  assert(words.length <= 3, `short response: "${resp.message.content}"`);
});

await test("Chat — auto-inferred provider", async () => {
  const resp = await client.chat("claude-sonnet-4-6", "Say ok.", {
    max_tokens: 100,
  });
  assert(resp.provider === "anthropic", "auto-detected anthropic");
});

await test("Chat — OpenAI", async () => {
  const resp = await client.chat("openai/gpt-5.4", "Say 'pong'.", {
    max_tokens: 100,
  });
  assert(resp.provider === "openai", `provider: ${resp.provider}`);
  assert(!!resp.message.content, "has content");
});

await test("Chat — Gemini", async () => {
  const resp = await client.chat(
    "gemini/gemini-3-flash-preview",
    "Say 'pong'.",
    { max_tokens: 200 }
  );
  assert(resp.provider === "gemini", `provider: ${resp.provider}`);
  assert(!!resp.message.content, "has content");
});

await test("Chat — with reasoning (provider_config)", async () => {
  const resp = await client.chat(
    "anthropic/claude-sonnet-4-6",
    "What is 5+3?",
    { max_tokens: 4000 },
    { thinking: { type: "enabled", budget_tokens: 2000 } }
  );
  assert(!!resp.reasoning, "has reasoning content");
  assert(
    (resp.message.content as string).includes("8"),
    `answer contains 8: ${resp.message.content}`
  );
});

await test("Chat — multi-model conversation", async () => {
  const messages: Message[] = [
    { role: "user", content: "Pick a color. One word only." },
  ];
  const r1 = await client.chat("anthropic/claude-sonnet-4-6", messages, {
    max_tokens: 100,
  });
  assert(!!r1.message.content, `Claude: ${r1.message.content}`);

  messages.push({ role: "assistant", content: r1.message.content as string });
  messages.push({
    role: "user",
    content: "Name a fruit that color. One word only.",
  });

  const r2 = await client.chat("openai/gpt-5.4", messages, {
    max_tokens: 100,
  });
  assert(!!r2.message.content, `GPT: ${r2.message.content}`);
  assert(r2.provider === "openai", "switched to openai");
});

await test("Stream — basic", async () => {
  let fullText = "";
  let gotDone = false;
  let gotUsage = false;

  for await (const chunk of client.stream(
    "anthropic/claude-sonnet-4-6",
    "Count from 1 to 3.",
    { max_tokens: 200 }
  )) {
    if (chunk.type === "content" && chunk.text) {
      fullText += chunk.text;
    }
    if (chunk.type === "done") gotDone = true;
    if (chunk.type === "usage") gotUsage = true;
  }

  assert(fullText.includes("1") && fullText.includes("3"), `got: ${fullText.slice(0, 50)}`);
  assert(gotDone, "received done event");
  assert(gotUsage, "received usage event");
});

await test("Stream — with reasoning", async () => {
  let reasoning = "";
  let content = "";

  for await (const chunk of client.stream(
    "anthropic/claude-sonnet-4-6",
    "What is 2+2?",
    { max_tokens: 4000, reasoning_effort: "high" }
  )) {
    if (chunk.type === "reasoning" && chunk.text) reasoning += chunk.text;
    if (chunk.type === "content" && chunk.text) content += chunk.text;
  }

  assert(reasoning.length > 0, `got reasoning: ${reasoning.slice(0, 50)}...`);
  assert(content.length > 0, `got content: ${content.slice(0, 50)}`);
});

await test("Error — unknown provider", async () => {
  try {
    await client.chat("unknown/model", "hi", { max_tokens: 100 });
    assert(false, "should have thrown");
  } catch (e: any) {
    assert(e.message.includes("unknown_provider"), `error: ${e.message}`);
  }
});

console.log(`\n${"=".repeat(40)}`);
console.log(`Results: ${passed} passed, ${failed} failed`);
process.exit(failed > 0 ? 1 : 0);
