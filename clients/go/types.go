package llmshim

import "fmt"

// ChatRequest is the request body for POST /v1/chat and POST /v1/chat/stream.
type ChatRequest struct {
	// Model identifier: "provider/model" (e.g. "anthropic/claude-sonnet-4-6")
	// or just the model name for auto-detection (e.g. "claude-sonnet-4-6").
	Model string `json:"model"`

	// Messages is the conversation history.
	Messages []Message `json:"messages"`

	// Stream, when true on POST /v1/chat, returns an SSE stream instead of
	// JSON. The Stream method sets this automatically; you normally leave it
	// unset.
	Stream bool `json:"stream,omitempty"`

	// Config holds provider-agnostic settings.
	Config *Config `json:"config,omitempty"`

	// ProviderConfig is raw provider-specific JSON merged into the underlying
	// request (e.g. Anthropic thinking, Gemini safety settings, tools).
	ProviderConfig map[string]any `json:"provider_config,omitempty"`

	// Fallback is an ordered list of model IDs tried if the primary model
	// fails with a retryable error (429, 500, 502, 503).
	Fallback []string `json:"fallback,omitempty"`
}

// Message is a single conversation message.
type Message struct {
	// Role is one of: system, user, assistant, tool, developer.
	Role string `json:"role"`

	// Content is either a string, an array of content blocks, or null.
	// For a simple text message pass a plain string.
	Content any `json:"content,omitempty"`

	// ToolCallID, on a "tool" role message, is the ID of the tool call being
	// responded to.
	ToolCallID string `json:"tool_call_id,omitempty"`

	// ToolCalls are tool calls made by the assistant.
	ToolCalls []ToolCall `json:"tool_calls,omitempty"`
}

// Config holds provider-agnostic configuration. Fields are pointers so that
// unset values are omitted from the request.
type Config struct {
	MaxTokens   *int     `json:"max_tokens,omitempty"`
	Temperature *float64 `json:"temperature,omitempty"`
	TopP        *float64 `json:"top_p,omitempty"`
	TopK        *int     `json:"top_k,omitempty"`
	Stop        []string `json:"stop,omitempty"`
	// ReasoningEffort controls reasoning/thinking depth: "none", "low",
	// "medium", "high", "xhigh", or "max". llmshim maps each value to the
	// target provider/model's native control, clamping to the nearest
	// supported tier (see docs/reasoning.md).
	ReasoningEffort string `json:"reasoning_effort,omitempty"`
	// ReasoningMode is "standard" (default) or "pro". "pro" requests
	// substantially more model work: native on OpenAI gpt-5.6/-pro models,
	// emulated as a one-tier effort bump elsewhere.
	ReasoningMode string `json:"reasoning_mode,omitempty"`
}

// ChatResponse is the response body from POST /v1/chat.
type ChatResponse struct {
	ID       string          `json:"id"`
	Model    string          `json:"model"`
	Provider string          `json:"provider"`
	Message  ResponseMessage `json:"message"`
	// Reasoning is the model's thinking content, if any.
	Reasoning *string `json:"reasoning,omitempty"`
	Usage     Usage   `json:"usage"`
	LatencyMs int64   `json:"latency_ms"`
}

// ResponseMessage is the assistant message in a ChatResponse. Content is a
// string or null.
type ResponseMessage struct {
	Role      string     `json:"role"`
	Content   any        `json:"content"`
	ToolCalls []ToolCall `json:"tool_calls,omitempty"`
}

// ToolCall is a function tool call.
type ToolCall struct {
	ID       string           `json:"id,omitempty"`
	Type     string           `json:"type,omitempty"`
	Function ToolCallFunction `json:"function"`
}

// ToolCallFunction is the function payload of a ToolCall.
type ToolCallFunction struct {
	Name string `json:"name"`
	// Arguments is a JSON-encoded string of arguments.
	Arguments string `json:"arguments"`
}

// Usage reports token counts for a request.
type Usage struct {
	InputTokens     int `json:"input_tokens"`
	OutputTokens    int `json:"output_tokens"`
	ReasoningTokens int `json:"reasoning_tokens,omitempty"`
	TotalTokens     int `json:"total_tokens"`
}

// StreamEventType enumerates the SSE event types.
type StreamEventType string

const (
	EventContent   StreamEventType = "content"
	EventReasoning StreamEventType = "reasoning"
	EventToolCall  StreamEventType = "tool_call"
	EventUsage     StreamEventType = "usage"
	EventDone      StreamEventType = "done"
	EventError     StreamEventType = "error"
)

// StreamEvent is a single event emitted while streaming. The Type field
// selects which of the other fields are populated:
//
//	content    → Text
//	reasoning  → Text
//	tool_call  → ID, Name, Arguments
//	usage      → InputTokens, OutputTokens, ReasoningTokens, TotalTokens
//	done       → (no payload)
//	error      → Message
//
// If a transport or parse error occurs while reading the stream, Err is set
// and the channel is then closed.
type StreamEvent struct {
	Type StreamEventType `json:"type"`

	// content, reasoning
	Text string `json:"text,omitempty"`

	// tool_call
	ID        string `json:"id,omitempty"`
	Name      string `json:"name,omitempty"`
	Arguments string `json:"arguments,omitempty"`

	// usage
	InputTokens     int `json:"input_tokens,omitempty"`
	OutputTokens    int `json:"output_tokens,omitempty"`
	ReasoningTokens int `json:"reasoning_tokens,omitempty"`
	TotalTokens     int `json:"total_tokens,omitempty"`

	// error
	Message string `json:"message,omitempty"`

	// Err is set for client-side transport or parse errors (not part of the
	// wire format).
	Err error `json:"-"`
}

// ModelsResponse is the response body from GET /v1/models.
type ModelsResponse struct {
	Models []Model `json:"models"`
}

// Model describes an available model.
type Model struct {
	// ID is the full identifier (provider/name).
	ID       string `json:"id"`
	Provider string `json:"provider"`
	// Name is the model name without the provider prefix.
	Name string `json:"name"`
}

// HealthResponse is the response body from GET /health.
type HealthResponse struct {
	Status    string   `json:"status"`
	Providers []string `json:"providers"`
}

// ErrorResponse is the wire format of an error body.
type ErrorResponse struct {
	Error ErrorDetail `json:"error"`
}

// ErrorDetail is the inner payload of an ErrorResponse.
type ErrorDetail struct {
	Code    string `json:"code"`
	Message string `json:"message"`
}

// APIError is returned for non-2xx responses. It carries the HTTP status code
// and, when the body parses as an ErrorResponse, the provider code and message.
type APIError struct {
	StatusCode int
	Code       string
	Message    string
}

// Error implements the error interface.
func (e *APIError) Error() string {
	if e.Code != "" {
		return fmt.Sprintf("llmshim: %d %s: %s", e.StatusCode, e.Code, e.Message)
	}
	return fmt.Sprintf("llmshim: %d: %s", e.StatusCode, e.Message)
}
