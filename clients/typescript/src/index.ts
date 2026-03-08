/**
 * llmshim TypeScript client — generated from OpenAPI spec.
 *
 * Usage:
 *   import { LlmShim } from "./src/index.ts";
 *   const client = new LlmShim("http://localhost:3000");
 *   const resp = await client.chat("claude-sonnet-4-6", "Hello!");
 */

import createClient from "openapi-fetch";
import type { paths, components } from "./api.d.ts";

// Re-export types for consumers
export type ChatRequest = components["schemas"]["ChatRequest"];
export type ChatResponse = components["schemas"]["ChatResponse"];
export type Message = components["schemas"]["Message"];
export type Config = components["schemas"]["Config"];
export type Usage = components["schemas"]["Usage"];
export type StreamEvent = components["schemas"]["StreamEvent"];
export type ModelEntry = components["schemas"]["ModelsResponse"]["models"][number];

export interface StreamChunk {
  type: "content" | "reasoning" | "tool_call" | "usage" | "done" | "error";
  text?: string;
  message?: string;
  id?: string;
  name?: string;
  arguments?: string;
  input_tokens?: number;
  output_tokens?: number;
  reasoning_tokens?: number;
  total_tokens?: number;
}

export class LlmShim {
  private client: ReturnType<typeof createClient<paths>>;
  private baseUrl: string;

  constructor(baseUrl: string = "http://localhost:3000") {
    this.baseUrl = baseUrl;
    this.client = createClient<paths>({ baseUrl });
  }

  /**
   * Send a chat completion request.
   * Convenience: pass a string for a single user message, or a full message array.
   */
  async chat(
    model: string,
    messages: string | Message[],
    config?: Config,
    providerConfig?: Record<string, unknown>
  ): Promise<ChatResponse> {
    const msgArray: Message[] =
      typeof messages === "string"
        ? [{ role: "user", content: messages }]
        : messages;

    const { data, error } = await this.client.POST("/v1/chat", {
      body: {
        model,
        messages: msgArray,
        config,
        provider_config: providerConfig,
      },
    });

    if (error) {
      throw new Error(`llmshim error: ${error.error.code} — ${error.error.message}`);
    }

    return data;
  }

  /**
   * Stream a chat completion. Yields typed events.
   */
  async *stream(
    model: string,
    messages: string | Message[],
    config?: Config,
    providerConfig?: Record<string, unknown>
  ): AsyncGenerator<StreamChunk> {
    const msgArray: Message[] =
      typeof messages === "string"
        ? [{ role: "user", content: messages }]
        : messages;

    const resp = await fetch(`${this.baseUrl}/v1/chat/stream`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        model,
        messages: msgArray,
        config,
        provider_config: providerConfig,
      }),
    });

    if (!resp.ok) {
      const err = await resp.json();
      throw new Error(`llmshim error: ${err.error.code} — ${err.error.message}`);
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
          yield { ...data, type: currentEvent || data.type } as StreamChunk;
        }
      }
    }
  }

  /**
   * List available models.
   */
  async models(): Promise<ModelEntry[]> {
    const { data, error } = await this.client.GET("/v1/models");
    if (error) throw new Error("Failed to list models");
    return data.models;
  }

  /**
   * Health check.
   */
  async health(): Promise<{ status: string; providers: string[] }> {
    const { data, error } = await this.client.GET("/health");
    if (error) throw new Error("Health check failed");
    return data;
  }
}

// Default export for convenience
export default LlmShim;
