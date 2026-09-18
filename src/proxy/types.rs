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

    /// Fallback models to try if the primary model fails (ordered).
    /// On retryable errors (429, 500, 502, 503), tries the next model in the list.
    #[serde(default)]
    pub fallback: Option<Vec<String>>,
    #[serde(default, rename = "x-cache")]
    pub cache: Option<crate::cache::CachePolicy>,
    #[serde(default, rename = "x-shim")]
    pub shim: Option<crate::shim::Config>,
    #[serde(default)]
    pub response_format: Option<Value>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<Value>,
    /// Preserve normalized replay metadata and legacy input fields verbatim.
    #[serde(default, flatten)]
    pub extra: serde_json::Map<String, Value>,
}

#[derive(Debug, Deserialize)]
pub struct Config {
    pub max_tokens: Option<u64>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub top_k: Option<u64>,
    pub stop: Option<Vec<String>>,
    /// Unified reasoning depth: none|low|medium|high|xhigh|max.
    /// Mapped per provider/model with clamping — see docs/src/guides/reasoning.md.
    pub reasoning_effort: Option<String>,
    /// Unified reasoning mode: standard (default) | pro. Native on OpenAI
    /// gpt-5.6/-pro models; emulated as a one-tier effort bump elsewhere.
    pub reasoning_mode: Option<String>,
}

// ============================================================
// Response types
// ============================================================

#[derive(Debug, Serialize)]
pub struct ChatResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
    #[serde(
        rename = "x-llmshim-served-model",
        skip_serializing_if = "Option::is_none"
    )]
    pub served_model: Option<String>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refusal: Option<String>,
    pub content: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<Value>,
}

#[derive(Debug, Serialize, Clone)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(skip_serializing_if = "is_zero")]
    pub reasoning_tokens: u64,
    pub total_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    /// USD charged for this response. `null` means the catalog carries no price
    /// for the model — it never means free. See `llmshim::cost`.
    pub cost_usd: Option<f64>,
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
    Reasoning { text: String, blocks: Vec<Value> },

    #[serde(rename = "tool_call")]
    ToolCall {
        id: String,
        name: String,
        arguments: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        thought_signature: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        wire_ids: Option<Value>,
    },

    #[serde(rename = "usage")]
    Usage(Usage),

    #[serde(rename = "done")]
    Done {
        #[serde(skip_serializing_if = "Option::is_none")]
        finish_reason: Option<String>,
        #[serde(
            rename = "x-llmshim-served-model",
            skip_serializing_if = "Option::is_none"
        )]
        served_model: Option<String>,
    },

    #[serde(rename = "error")]
    Error { message: String },
}

// ============================================================
// Models endpoint
// ============================================================

/// `GET /v1/models` answers two audiences from one body.
///
/// An OpenAI SDK reads `object` + `data` and ignores everything else, so
/// `client.models.list()` works against llmshim unmodified. The bundled
/// Python/TS/Go/Ruby clients read `models`, which is byte-for-byte what they
/// read before. There is no second path to split on — both audiences issue the
/// same `GET /v1/models` — so the union is the split.
#[derive(Debug, Serialize)]
pub struct ModelsResponse {
    /// Always `"list"`, per the OpenAI list envelope.
    pub object: &'static str,
    /// OpenAI-shaped entries.
    pub data: Vec<ModelObject>,
    /// llmshim's own shape. Unchanged; existing clients keep reading this.
    pub models: Vec<ModelEntry>,
}

/// One entry in the OpenAI `data` array. `id` is the routing id llmshim
/// accepts back as `model`, so a listed id is directly requestable.
#[derive(Debug, Serialize)]
pub struct ModelObject {
    pub id: String,
    /// Always `"model"`.
    pub object: &'static str,
    /// Release date as a Unix timestamp, or `0` when the catalog has none.
    /// OpenAI clients type this as an integer, so it is never null.
    pub created: i64,
    /// The provider serving the model.
    pub owned_by: String,
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
