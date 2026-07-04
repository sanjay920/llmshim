/**
 * TypeScript types mirroring the llmshim proxy OpenAPI schema (api/openapi.yaml).
 */

/** Reasoning/thinking depth, applied across all providers. */
export type ReasoningEffort = "low" | "medium" | "high";

/** Role of a conversation message. */
export type Role = "system" | "user" | "assistant" | "tool" | "developer";

/** A tool call made by the assistant. */
export interface ToolCall {
  id?: string;
  type?: "function";
  function?: {
    name?: string;
    /** JSON-encoded arguments. */
    arguments?: string;
  };
}

/** A conversation message sent to the proxy. */
export interface Message {
  role: Role;
  /** Text content, an array of content blocks, or null. */
  content?: string | Array<Record<string, unknown>> | null;
  /** For `tool` role messages, the ID of the tool call being responded to. */
  tool_call_id?: string;
  /** Tool calls made by the assistant. */
  tool_calls?: ToolCall[];
}

/** Provider-agnostic configuration. */
export interface Config {
  /** Maximum output tokens. */
  max_tokens?: number;
  /** Sampling temperature (0–2). */
  temperature?: number;
  top_p?: number;
  top_k?: number;
  stop?: string[];
  /** Controls reasoning/thinking depth across all providers. */
  reasoning_effort?: ReasoningEffort;
}

/** Request body for POST /v1/chat and POST /v1/chat/stream. */
export interface ChatRequest {
  /**
   * Model identifier. Use "provider/model" (e.g. "anthropic/claude-sonnet-4-6")
   * or just the model name for auto-detection (e.g. "claude-sonnet-4-6").
   */
  model: string;
  /** Conversation messages. */
  messages: Message[];
  /** If true on /v1/chat, returns an SSE stream instead of JSON. */
  stream?: boolean;
  /** Provider-agnostic configuration. */
  config?: Config;
  /** Raw provider-specific JSON merged into the underlying request. */
  provider_config?: Record<string, unknown>;
  /** Ordered list of fallback model IDs tried on retryable errors. */
  fallback?: string[];
}

/** Token usage reported by the provider. */
export interface Usage {
  input_tokens?: number;
  output_tokens?: number;
  /** Reasoning/thinking tokens used (if applicable). */
  reasoning_tokens?: number;
  total_tokens?: number;
}

/** The assistant message inside a ChatResponse. */
export interface ResponseMessage {
  role: string;
  content: string | null;
  tool_calls?: ToolCall[];
}

/** Response body from POST /v1/chat (non-streaming). */
export interface ChatResponse {
  /** Response ID from the provider. */
  id: string;
  model: string;
  /** Which provider handled the request. */
  provider: string;
  message: ResponseMessage;
  /** Reasoning/thinking content if the model produced it. */
  reasoning?: string | null;
  usage: Usage;
  /** End-to-end latency in milliseconds. */
  latency_ms: number;
}

/** A chunk of answer text. */
export interface ContentEvent {
  type: "content";
  text: string;
}

/** A chunk of reasoning/thinking text. */
export interface ReasoningEvent {
  type: "reasoning";
  text: string;
}

/** A tool call emitted during streaming. */
export interface ToolCallEvent {
  type: "tool_call";
  id?: string;
  name?: string;
  /** JSON-encoded arguments. */
  arguments?: string;
}

/** Final token usage, emitted near the end of a stream. */
export interface UsageEvent {
  type: "usage";
  input_tokens?: number;
  output_tokens?: number;
  reasoning_tokens?: number;
  total_tokens?: number;
}

/** Terminal event signalling the stream is complete. */
export interface DoneEvent {
  type: "done";
}

/** An error surfaced mid-stream. */
export interface ErrorEvent {
  type: "error";
  message: string;
}

/** Discriminated union of all SSE events emitted during streaming. */
export type StreamEvent =
  | ContentEvent
  | ReasoningEvent
  | ToolCallEvent
  | UsageEvent
  | DoneEvent
  | ErrorEvent;

/** A single entry in the /v1/models response. */
export interface ModelInfo {
  /** Full model identifier (provider/name). */
  id: string;
  provider: string;
  /** Model name without provider prefix. */
  name: string;
}

/** Response body from GET /v1/models. */
export interface ModelsResponse {
  models: ModelInfo[];
}

/** Response body from GET /health. */
export interface HealthResponse {
  status: string;
  /** List of configured providers. */
  providers: string[];
}

/** Error envelope returned on non-2xx responses. */
export interface ErrorResponse {
  error: {
    code: string;
    message: string;
  };
}
