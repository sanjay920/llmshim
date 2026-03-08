use serde::{Deserialize, Serialize};
use serde_json::Value;

// ============================================================
// Request types
// ============================================================

#[derive(Debug, Deserialize)]
pub struct ChatRequest {
    /// Model identifier: "provider/model" or auto-detected (e.g., "claude-sonnet-4-6")
    pub model: String,

    /// Conversation messages
    pub messages: Vec<Message>,

    /// Whether to stream the response (only on /v1/chat)
    #[serde(default)]
    pub stream: bool,

    /// Provider-agnostic configuration
    #[serde(default)]
    pub config: Option<Config>,

    /// Raw provider-specific JSON, merged into the underlying request
    #[serde(default)]
    pub provider_config: Option<Value>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Message {
    pub role: String,

    #[serde(default)]
    pub content: Value,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Value>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Config {
    pub max_tokens: Option<u64>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub top_k: Option<u64>,
    pub stop: Option<Vec<String>>,
    pub reasoning_effort: Option<String>,
}

// ============================================================
// Response types
// ============================================================

#[derive(Debug, Serialize)]
pub struct ChatResponse {
    pub id: String,
    pub model: String,
    pub provider: String,
    pub message: ResponseMessage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    pub usage: Usage,
    pub latency_ms: u64,
}

#[derive(Debug, Serialize)]
pub struct ResponseMessage {
    pub role: String,
    pub content: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Value>,
}

#[derive(Debug, Serialize, Clone)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(skip_serializing_if = "is_zero")]
    pub reasoning_tokens: u64,
    pub total_tokens: u64,
}

fn is_zero(v: &u64) -> bool {
    *v == 0
}

// ============================================================
// SSE stream event types
// ============================================================

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
pub enum StreamEvent {
    #[serde(rename = "content")]
    Content { text: String },

    #[serde(rename = "reasoning")]
    Reasoning { text: String },

    #[serde(rename = "tool_call")]
    ToolCall {
        id: String,
        name: String,
        arguments: String,
    },

    #[serde(rename = "usage")]
    Usage(Usage),

    #[serde(rename = "done")]
    Done {},

    #[serde(rename = "error")]
    Error { message: String },
}

// ============================================================
// Models endpoint
// ============================================================

#[derive(Debug, Serialize)]
pub struct ModelsResponse {
    pub models: Vec<ModelEntry>,
}

#[derive(Debug, Serialize)]
pub struct ModelEntry {
    pub id: String,
    pub provider: String,
    pub name: String,
}

// ============================================================
// Health endpoint
// ============================================================

#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub providers: Vec<String>,
}

// ============================================================
// Error response
// ============================================================

#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub error: ErrorDetail,
}

#[derive(Debug, Serialize)]
pub struct ErrorDetail {
    pub code: String,
    pub message: String,
}
