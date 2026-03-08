/**
 * llmshim JavaScript (Node.js/Bun) client example.
 *
 * Usage:
 *   cargo run --features proxy --bin llmshim-proxy &
 *   node examples/clients/javascript_client.mjs
 */

const BASE_URL = "http://localhost:3000";

async function chat(model, messages, config) {
  const resp = await fetch(`${BASE_URL}/v1/chat`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ model, messages, config }),
  });
  if (!resp.ok) {
    const err = await resp.json();
    throw new Error(`${err.error.code}: ${err.error.message}`);
  }
  return resp.json();
}

async function* chatStream(model, messages, config) {
  const resp = await fetch(`${BASE_URL}/v1/chat/stream`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ model, messages, config }),
  });

  const reader = resp.body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";
  let event = "";

  while (true) {
    const { done, value } = await reader.read();
    if (done) break;
    buffer += decoder.decode(value, { stream: true });
    const lines = buffer.split("\n");
    buffer = lines.pop();
    for (const line of lines) {
      if (line.startsWith("event: ")) event = line.slice(7);
      else if (line.startsWith("data: ")) yield { event, data: JSON.parse(line.slice(6)) };
    }
  }
}

// Demo
const result = await chat(
  "anthropic/claude-sonnet-4-6",
  [{ role: "user", content: "Say hello in one word." }],
  { max_tokens: 100 }
);
console.log(`${result.provider}: ${result.message.content} (${result.latency_ms}ms)`);

process.stdout.write("\nStreaming: ");
for await (const { event, data } of chatStream(
  "gemini/gemini-3-flash-preview",
  [{ role: "user", content: "Count 1 to 5." }],
  { max_tokens: 200 }
)) {
  if (event === "content") process.stdout.write(data.text);
  if (event === "done") console.log(" ✓");
}
