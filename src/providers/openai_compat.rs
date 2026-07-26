use crate::error::{Result, ShimError};
use crate::provider::{Provider, ProviderRequest};
use crate::vision;
use serde_json::{json, Value};

/// A generic OpenAI Chat Completions-compatible provider for **self-hosted**
/// inference servers — vLLM and SGLang. Like OpenRouter it's a passthrough
/// (messages, tools, `image_url` vision, and `response_format` are already in
/// the target shape), but two things differ from a hosted aggregator:
///
/// - **The base URL is configuration**, not a constant — that's what "local vs
///   remote" means (`http://localhost:8000/v1` vs `https://host/v1`).
/// - **Auth is optional** — these servers accept unauthenticated requests unless
///   launched with `--api-key`, so the `Authorization` header is sent only when
///   a key is configured.
///
/// `name` (e.g. `"vllm"` / `"sglang"`) is both the provider key and the
/// extension namespace: server-specific params (`chat_template_kwargs`,
/// `separate_reasoning`, `guided_json`, `top_k`, …) go under `x-<name>`.
pub struct OpenAiCompatible {
    pub name: String,
    pub base_url: String,
    pub api_key: Option<String>,
}

impl OpenAiCompatible {
    pub fn new(
        name: impl Into<String>,
        base_url: impl Into<String>,
        api_key: Option<String>,
    ) -> Self {
        Self {
            name: name.into(),
            base_url: base_url.into(),
            api_key,
        }
    }
}

/// Strip llmshim-normalized / foreign-provider fields and normalize content
/// blocks to Chat Completions form. Messages, `tool_calls`, and `role: "tool"`
/// stay in Chat Completions shape (the target format).
fn sanitize_messages(messages: &[Value]) -> Vec<Value> {
    messages
        .iter()
        .map(|msg| {
            let mut out = msg.clone();
            if let Some(obj) = out.as_object_mut() {
                obj.remove("reasoning_content"); // regenerated server-side; don't echo back
                obj.remove("reasoning_signature"); // opaque Anthropic token — never forward
                obj.remove("redacted_reasoning_content"); // opaque Anthropic token — never forward
                obj.remove("annotations");
                obj.remove("refusal");
            }
            if let Some(content) = out.get("content").cloned() {
                if content.is_array() {
                    let translated =
                        vision::translate_content_blocks(&content, vision::to_openai_chat);
                    out["content"] = vision::text_blocks_to_chat(&translated);
                }
            }
            out
        })
        .collect()
}

/// Copy OpenRouter/vLLM/SGLang's `reasoning` field into llmshim's
/// `reasoning_content` convention if the latter isn't already present. (vLLM is
/// migrating `reasoning_content` → `reasoning`; SGLang uses `reasoning_content`.)
fn normalize_reasoning(obj: &mut serde_json::Map<String, Value>) {
    if obj.contains_key("reasoning_content") {
        return;
    }
    if let Some(r) = obj
        .get("reasoning")
        .and_then(|r| r.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
    {
        obj.insert("reasoning_content".to_string(), json!(r));
    }
}

impl Provider for OpenAiCompatible {
    fn name(&self) -> &str {
        &self.name
    }

    fn transform_request(&self, model: &str, request: &Value) -> Result<ProviderRequest> {
        let obj = request.as_object().ok_or(ShimError::MissingModel)?;
        let messages = obj
            .get("messages")
            .and_then(|m| m.as_array())
            .ok_or(ShimError::MissingModel)?;

        let mut body = json!({
            "model": model,
            "messages": sanitize_messages(messages),
        });
        let body_obj = body.as_object_mut().unwrap();

        // Standard Chat Completions params (plus reasoning_effort, which vLLM and
        // some SGLang models honor natively) — forwarded unchanged.
        for key in [
            "max_tokens",
            "max_completion_tokens",
            "temperature",
            "top_p",
            "frequency_penalty",
            "presence_penalty",
            "stop",
            "seed",
            "stream",
            "stream_options",
            "tools",
            "tool_choice",
            "parallel_tool_calls",
            "response_format",
            "logprobs",
            "top_logprobs",
            "n",
            "reasoning_effort",
        ] {
            if let Some(v) = obj.get(key) {
                body_obj.insert(key.to_string(), v.clone());
            }
        }

        // x-<name> namespace: server-specific params (sampling knobs like top_k /
        // min_p, guided_json / regex / ebnf, chat_template_kwargs,
        // separate_reasoning, …) are copied straight into the body.
        let ns = format!("x-{}", self.name);
        if let Some(ext) = obj.get(&ns).and_then(|e| e.as_object()) {
            for (k, v) in ext {
                body_obj.insert(k.clone(), v.clone());
            }
        }

        let mut headers = vec![("Content-Type".to_string(), "application/json".to_string())];
        // Auth is optional — self-hosted servers are unauthenticated unless
        // launched with --api-key.
        if let Some(key) = &self.api_key {
            if !key.is_empty() {
                headers.push(("Authorization".to_string(), format!("Bearer {key}")));
            }
        }

        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        Ok(ProviderRequest { url, headers, body })
    }

    fn transform_response(&self, _model: &str, mut response: Value) -> Result<Value> {
        if let Some(err) = response.get("error") {
            if !err.is_null() {
                let message = err
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown error")
                    .to_string();
                let status = err.get("code").and_then(|c| c.as_u64()).unwrap_or(400) as u16;
                return Err(ShimError::ProviderError {
                    status,
                    body: message,
                });
            }
        }

        // Already Chat Completions-shaped. Normalize the reasoning field name.
        if let Some(choices) = response.get_mut("choices").and_then(|c| c.as_array_mut()) {
            for choice in choices {
                if let Some(msg) = choice.get_mut("message").and_then(|m| m.as_object_mut()) {
                    normalize_reasoning(msg);
                }
            }
        }

        Ok(response)
    }

    fn transform_stream_chunk(&self, _model: &str, chunk: &str) -> Result<Option<String>> {
        let mut parsed: Value = match serde_json::from_str(chunk) {
            Ok(v) => v,
            Err(_) => return Ok(None),
        };

        if let Some(choices) = parsed.get_mut("choices").and_then(|c| c.as_array_mut()) {
            for choice in choices {
                if let Some(delta) = choice.get_mut("delta").and_then(|d| d.as_object_mut()) {
                    normalize_reasoning(delta);
                }
            }
        }

        Ok(Some(serde_json::to_string(&parsed)?))
    }
}
