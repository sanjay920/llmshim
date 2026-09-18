/**
 * TypeScript types mirroring the llmshim proxy OpenAPI schema (api/openapi.yaml).
 */

/** Reasoning/thinking depth, applied across all providers. */
export type ReasoningEffort = "none" | "low" | "medium" | "high" | "xhigh" | "max";

/**
 * Unified reasoning mode. "pro" requests substantially more model work:
 * native `reasoning.mode` on OpenAI gpt-5.6/-pro models, emulated as a
 * one-tier effort bump on all other models/providers.
 */
export type ReasoningMode = "standard" | "pro";

/** Role of a conversation message. */
export type Role = "system" | "user" | "assistant" | "tool" | "developer";

/** Provenance recorded when a reasoning block was received. */
export interface ReasoningOrigin {
  provider: string;
  model: string;
  family: string | null;
  wire: "anthropic-messages" | "openai-responses" | "openai-chat" | "google-generate-content";
  received_at: string;
  account?: string;
}

/** Preserve the complete object when recording or replaying a conversation. */
export interface ReasoningBlock {
  kind: "text" | "redacted" | "encrypted";
  text?: string;
  data?: string;
  signature?: string;
  item_id?: string;
  origin: ReasoningOrigin;
  payload?: unknown;
  source_field?: string;
}

export interface ReasoningDelta extends ReasoningBlock {
  index?: number | string;
  replace?: boolean;
}

export interface ThoughtSignature {
  data: string;
  origin: ReasoningOrigin;
}

/** Persist this mapping with the call; id is the wire correlation id. */
export interface WireToolId {
  signature_field?: string;
  provider: string;
  wire: ReasoningOrigin["wire"];
  scope: string;
  part_id: string;
  id: string | null;
  item_id?: string;
}

/** A tool call made by the assistant. */
export interface ToolCall {
  wire_ids?: WireToolId[];
  thought_signature?: ThoughtSignature;
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
  reasoning?: ReasoningBlock[];
  /**
   * @deprecated Legacy input only. Untracked reasoning is dropped; preserve
   * the full reasoning array from the previous assistant message instead.
   */
  reasoning_content?: string;
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
  /**
   * Unified reasoning/thinking depth across all providers. llmshim maps each
   * value to the target provider/model's native control, clamping to the
   * nearest supported tier (see docs/src/guides/reasoning.md).
   */
  reasoning_effort?: ReasoningEffort;
  /** Unified reasoning mode; see {@link ReasoningMode}. Default "standard". */
  reasoning_mode?: ReasoningMode;
}

export interface CacheSegment {
  upto_message: number;
  label?: string;
  stability: "static" | "session" | "turn";
}
export interface CachePolicy {
  segments?: CacheSegment[];
  key?: string;
}

export interface ShimConfig {
  structured_output?: "auto" | "native" | "forced_tool" | "prompt";
  tool_calling?: "auto" | "native" | "prompt";
  reasoning_capture?: "off" | "forced_tool";
}
export type ResponseFormat = {
  type: "json_schema";
  json_schema: { name?: string; schema: unknown; strict?: boolean };
} | { type: "json_object" };

/** Request body for POST /v1/chat and POST /v1/chat/stream. */
export interface ChatRequest {
  "x-cache"?: CachePolicy;
  "x-shim"?: ShimConfig;
  response_format?: ResponseFormat;
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
  input_tokens: number;
  output_tokens: number;
  /** Reasoning/thinking tokens used (omitted when zero). */
  reasoning_tokens?: number;
  total_tokens: number;
  /** Cache input tokens (absent on servers older than 0.4). */
  cache_read_tokens?: number;
  cache_write_tokens?: number;
}

/** The assistant message inside a ChatResponse. */
export interface ResponseMessage {
  refusal?: string;
  role: string;
  /**
   * Assistant content — a string for plain text, or an array of content
   * blocks for vision / structured tool output (an arbitrary JSON value).
   */
  content: unknown;
  tool_calls?: ToolCall[];
  reasoning?: ReasoningBlock[];
}

/** Response body from POST /v1/chat (non-streaming). */
export interface ChatResponse {
  finish_reason?: string;
  "x-llmshim-served-model"?: string;
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
  blocks?: ReasoningDelta[];
  type: "reasoning";
  text: string;
}

/** A tool call emitted during streaming. */
export interface ToolCallEvent {
  wire_ids?: WireToolId[];
  thought_signature?: ThoughtSignature;
  type: "tool_call";
  id: string;
  name: string;
  /** JSON-encoded arguments. */
  arguments: string;
}

/** Final token usage, emitted near the end of a stream. */
export interface UsageEvent {
  type: "usage";
  input_tokens: number;
  output_tokens: number;
  /** Reasoning/thinking tokens used (omitted when zero). */
  reasoning_tokens?: number;
  total_tokens: number;
  /** Cache input tokens (absent on servers older than 0.4). */
  cache_read_tokens?: number;
  cache_write_tokens?: number;
}

/** Terminal event signalling the stream is complete. */
export interface DoneEvent {
  finish_reason?: string;
  "x-llmshim-served-model"?: string;
  type: "done";
}

/** An error surfaced mid-stream. */
export interface ErrorEvent {
  type: "error";
  message: string;
  error?: {
    message: string;
    type?: string;
    code: unknown;
    param: unknown;
    status?: number;
  };
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
