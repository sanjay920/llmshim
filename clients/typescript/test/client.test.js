// Tests for the llmshim TypeScript client.
//
// These tests are FULLY MOCKED: an in-process http.createServer returns canned
// JSON and canned SSE streams. No real provider API is ever contacted and no
// running llmshim proxy is required — running this suite costs $0.

import assert from "node:assert/strict";
import { after, before, test } from "node:test";
import { createServer } from "node:http";

import { Client, LlmshimError, createClient } from "../dist/index.js";

/** @type {import('node:http').Server} */
let server;
/** @type {string} */
let baseUrl;
/** Handler swapped per test. @type {(req, res) => void} */
let handler = () => {};

before(async () => {
  server = createServer((req, res) => handler(req, res));
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  const addr = server.address();
  baseUrl = `http://127.0.0.1:${addr.port}`;
});

after(async () => {
  await new Promise((resolve) => server.close(resolve));
});

function json(res, status, body) {
  res.writeHead(status, { "content-type": "application/json" });
  res.end(JSON.stringify(body));
}

function readBody(req) {
  return new Promise((resolve) => {
    let data = "";
    req.on("data", (c) => (data += c));
    req.on("end", () => resolve(data ? JSON.parse(data) : undefined));
  });
}

test("chat() posts to /v1/chat and parses ChatResponse", async () => {
  let seenPath, seenBody;
  handler = async (req, res) => {
    seenPath = req.url;
    seenBody = await readBody(req);
    json(res, 200, {
      id: "resp_123",
      model: "claude-sonnet-4-6",
      provider: "anthropic",
      message: { role: "assistant", content: "Hello there!" },
      reasoning: null,
      usage: { input_tokens: 10, output_tokens: 5, total_tokens: 15 },
      latency_ms: 420,
    });
  };

  const client = new Client({ baseUrl });
  const res = await client.chat({
    model: "anthropic/claude-sonnet-4-6",
    messages: [{ role: "user", content: "Hi" }],
    config: { max_tokens: 100 },
  });

  assert.equal(seenPath, "/v1/chat");
  assert.equal(seenBody.model, "anthropic/claude-sonnet-4-6");
  assert.equal(seenBody.config.max_tokens, 100);
  assert.equal(res.id, "resp_123");
  assert.equal(res.provider, "anthropic");
  assert.equal(res.message.content, "Hello there!");
  assert.equal(res.usage.total_tokens, 15);
});

test("stream() yields typed events from a multi-event SSE stream", async () => {
  handler = (req, res) => {
    assert.equal(req.url, "/v1/chat/stream");
    res.writeHead(200, { "content-type": "text/event-stream" });
    // Multi-event stream, including a multi-line data field and a tool_call.
    res.write("event: reasoning\ndata: {\"text\": \"thinking...\"}\n\n");
    res.write("event: content\ndata: {\"text\": \"Hello\"}\n\n");
    res.write("event: content\ndata: {\"text\": \" world\"}\n\n");
    res.write(
      'event: tool_call\ndata: {"id": "call_1", "name": "get_weather",\ndata: "arguments": "{\\"city\\":\\"SF\\"}"}\n\n',
    );
    res.write(
      "event: usage\ndata: {\"input_tokens\": 3, \"output_tokens\": 2, \"total_tokens\": 5}\n\n",
    );
    res.write("event: done\ndata: {}\n\n");
    res.end();
  };

  const client = createClient({ baseUrl });
  const events = [];
  for await (const ev of client.stream({
    model: "anthropic/claude-sonnet-4-6",
    messages: [{ role: "user", content: "Hi" }],
  })) {
    events.push(ev);
  }

  assert.deepEqual(
    events.map((e) => e.type),
    ["reasoning", "content", "content", "tool_call", "usage", "done"],
  );
  assert.equal(events[0].text, "thinking...");
  assert.equal(events[1].text + events[2].text, "Hello world");
  assert.equal(events[3].name, "get_weather");
  assert.equal(events[3].arguments, '{"city":"SF"}');
  assert.equal(events[4].total_tokens, 5);
});

test("stream() honors [DONE] termination and split chunk boundaries", async () => {
  handler = (req, res) => {
    res.writeHead(200, { "content-type": "text/event-stream" });
    // Split a single event across writes to exercise buffering.
    res.write("event: content\ndata: {\"text\": \"par");
    res.write("tial\"}\n\n");
    res.write("data: [DONE]\n\n");
    res.end();
  };

  const client = new Client({ baseUrl });
  const events = [];
  for await (const ev of client.stream({
    model: "gpt-5.5",
    messages: [{ role: "user", content: "Hi" }],
  })) {
    events.push(ev);
  }

  assert.equal(events.length, 2);
  assert.equal(events[0].type, "content");
  assert.equal(events[0].text, "partial");
  assert.equal(events[1].type, "done");
});

test("models() parses ModelsResponse", async () => {
  handler = (req, res) => {
    assert.equal(req.url, "/v1/models");
    json(res, 200, {
      models: [
        { id: "anthropic/claude-sonnet-4-6", provider: "anthropic", name: "claude-sonnet-4-6" },
        { id: "openai/gpt-5.5", provider: "openai", name: "gpt-5.5" },
      ],
    });
  };

  const client = new Client({ baseUrl });
  const res = await client.models();
  assert.equal(res.models.length, 2);
  assert.equal(res.models[0].provider, "anthropic");
});

test("health() parses HealthResponse", async () => {
  handler = (req, res) => {
    assert.equal(req.url, "/health");
    json(res, 200, { status: "ok", providers: ["anthropic", "openai"] });
  };

  const client = new Client({ baseUrl });
  const res = await client.health();
  assert.equal(res.status, "ok");
  assert.deepEqual(res.providers, ["anthropic", "openai"]);
});

test("non-2xx ErrorResponse throws a typed LlmshimError", async () => {
  handler = (req, res) => {
    json(res, 400, { error: { code: "invalid_request", message: "missing model" } });
  };

  const client = new Client({ baseUrl });
  await assert.rejects(
    () => client.chat({ model: "", messages: [] }),
    (err) => {
      assert.ok(err instanceof LlmshimError);
      assert.equal(err.status, 400);
      assert.equal(err.code, "invalid_request");
      assert.equal(err.message, "missing model");
      return true;
    },
  );
});

test("non-JSON error body still throws LlmshimError with status", async () => {
  handler = (req, res) => {
    res.writeHead(502, { "content-type": "text/plain" });
    res.end("upstream boom");
  };

  const client = new Client({ baseUrl });
  await assert.rejects(
    () => client.health(),
    (err) => {
      assert.ok(err instanceof LlmshimError);
      assert.equal(err.status, 502);
      assert.equal(err.message, "upstream boom");
      return true;
    },
  );
});

test("custom headers are sent", async () => {
  let seenAuth;
  handler = (req, res) => {
    seenAuth = req.headers["authorization"];
    json(res, 200, { status: "ok", providers: [] });
  };

  const client = new Client({ baseUrl, headers: { authorization: "Bearer test" } });
  await client.health();
  assert.equal(seenAuth, "Bearer test");
});
