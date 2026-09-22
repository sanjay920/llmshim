use crate::error::{Result, ShimError};
use crate::provider::{Provider, ProviderRequest};
use crate::providers::openai::OpenAi;
use crate::reasoning::{ReplayTarget, WireFormat};
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
///
/// The wire is Chat Completions unless [`OpenAiCompatible::with_wire`] selects
/// the Responses API, which SGLang also serves at `<base>/responses`. On that
/// wire reasoning comes back as an item with its own id, so it can be replayed
/// as a keyed block instead of bare `reasoning_content`; the request and
/// response translation is the OpenAI adapter's, with this server's URL, its
/// optional auth, and its `x-<name>` namespace.
pub struct OpenAiCompatible {
    pub name: String,
    pub base_url: String,
    pub api_key: Option<String>,
    wire: WireFormat,
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
            wire: WireFormat::OpenAiChat,
        }
    }

    /// Select the wire this server is spoken to on: `OpenAiChat` (the
    /// default, `<base>/chat/completions`) or `OpenAiResponses`
    /// (`<base>/responses`). The other wires are not OpenAI-compatible and
    /// are a caller error.
    pub fn with_wire(mut self, wire: WireFormat) -> Self {
        assert!(
            matches!(wire, WireFormat::OpenAiChat | WireFormat::OpenAiResponses),
            "an OpenAI-compatible server speaks Chat Completions or Responses, not {wire:?}"
        );
        self.wire = wire;
        self
    }

    pub fn wire(&self) -> WireFormat {
        self.wire
    }

    /// Auth is optional — self-hosted servers are unauthenticated unless
    /// launched with --api-key.
    fn headers(&self) -> Vec<(String, String)> {
        let mut headers = vec![("Content-Type".to_string(), "application/json".to_string())];
        if let Some(key) = self.api_key.as_deref().filter(|k| !k.is_empty()) {
            headers.push(("Authorization".to_string(), format!("Bearer {key}")));
        }
        headers
    }

    /// The OpenAI adapter pointed at this server, for the Responses wire. Its
    /// translation is reused whole; only the URL, the auth and the extension
    /// namespace are this server's.
    fn responses_adapter(&self) -> OpenAi {
        OpenAi::new(self.api_key.clone().unwrap_or_default())
            .with_base_url(self.base_url.trim_end_matches('/').to_string())
    }

    fn transform_request_responses(&self, model: &str, request: &Value) -> Result<ProviderRequest> {
        // The OpenAI adapter reads its overrides from `x-openai`, and applies
        // them before the stateless and native-tool passes. Moving `x-<name>`
        // there keeps that order, so an override cannot re-enable storage.
        let mut schema_budget = crate::schema::RequestBudget::new();
        schema_budget.reserve_request_schemas(request)?;
        let mut request = request.clone();
        let namespace = format!("x-{}", self.name);
        if let Some(ext) = request
            .as_object_mut()
            .and_then(|obj| obj.remove(&namespace))
            .and_then(|ext| ext.as_object().cloned())
        {
            let target = request["x-openai"].as_object().cloned().unwrap_or_default();
            request["x-openai"] = Value::Object(target.into_iter().chain(ext).collect());
        }
        let mut sent = self
            .responses_adapter()
            .transform_request_for_target_with_budget(
                model,
                &request,
                &self.replay_target(model),
                &mut schema_budget,
            )?;
        sent.headers = self.headers();
        Ok(sent)
    }
}

/// Strip llmshim-normalized / foreign-provider fields and normalize content
/// blocks to Chat Completions form. Messages, `tool_calls`, and `role: "tool"`
/// stay in Chat Completions shape (the target format).
///
/// Same-role adjacency passes through unchanged, and here the answer is
/// genuinely the served model's. vLLM and SGLang render `messages` through the
/// tokenizer's Jinja chat template (or `--chat-template`), so acceptance is a
/// property of that template: most current ones accept adjacent turns, some
/// older ones raise — Mistral-7B-Instruct-v0.1's template errors with
/// "conversation roles must alternate user/assistant/user/assistant/...".
/// llmshim cannot see the template, so it does not merge; a strict template's
/// rejection surfaces as the server's own 400. No `x-vllm` / `x-sglang`
/// parameter changes this.
fn sanitize_messages(messages: &[Value]) -> Vec<Value> {
    messages
        .iter()
        .map(|msg| {
            let mut out = msg.clone();
            if let Some(obj) = out.as_object_mut() {
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

impl Provider for OpenAiCompatible {
    fn name(&self) -> &str {
        &self.name
    }

    fn request_admission_policy(&self) -> crate::provider::RequestAdmissionPolicy {
        match self.wire {
            WireFormat::OpenAiResponses => crate::provider::RequestAdmissionPolicy::namespaced(
                format!("x-{}", self.name),
                &["model", "input"],
                &[
                    "instructions",
                    "prompt",
                    "text",
                    "tools",
                    "tool_choice",
                    "guided_json",
                    "guided_regex",
                    "guided_ebnf",
                    "structured_outputs",
                    "chat_template_kwargs",
                ],
                &["max_output_tokens"],
            ),
            WireFormat::OpenAiChat => crate::provider::RequestAdmissionPolicy::namespaced(
                format!("x-{}", self.name),
                &["model", "messages"],
                &[
                    "tools",
                    "tool_choice",
                    "response_format",
                    "guided_json",
                    "guided_regex",
                    "guided_ebnf",
                    "structured_outputs",
                    "chat_template_kwargs",
                ],
                &["max_tokens", "max_completion_tokens"],
            ),
            _ => crate::provider::RequestAdmissionPolicy::default(),
        }
    }

    fn replay_target(&self, model: &str) -> ReplayTarget {
        ReplayTarget::new(self.name(), model, self.wire)
            .bind_account(&self.base_url, self.api_key.as_deref())
    }

    fn transform_request(&self, model: &str, request: &Value) -> Result<ProviderRequest> {
        crate::reasoning::preflight_request(request)?;
        if self.wire == WireFormat::OpenAiResponses {
            return self.transform_request_responses(model, request);
        }
        let mut schema_budget = crate::schema::RequestBudget::new();
        let request = crate::schema::prepare_request(request, &mut schema_budget)?;
        let request =
            crate::cache::prepare_request(&request, crate::reasoning::WireFormat::OpenAiChat)?;
        let request = crate::reasoning::prepare_request(&request, &self.replay_target(model))?;
        let request = crate::toolcall::prepare_request(&request, &self.replay_target(model))?;
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

        let headers = self.headers();

        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        crate::toolcall::validate_native(&body, &self.replay_target(model))?;
        crate::schema::normalize_native_tools(
            crate::schema::Target::OpenAiChat,
            &mut body,
            &mut schema_budget,
        )?;
        crate::shim::native_format(
            &request,
            crate::reasoning::WireFormat::OpenAiChat,
            &mut body,
            &mut schema_budget,
        )?;
        crate::cache::finish_request(
            &request,
            &mut body,
            crate::reasoning::WireFormat::OpenAiChat,
        )?;
        Ok(ProviderRequest { url, headers, body })
    }

    fn transform_response(&self, model: &str, response: Value) -> Result<Value> {
        let native = response.clone();
        let mut result = match self.wire {
            WireFormat::OpenAiResponses => self
                .responses_adapter()
                .transform_response_native(model, response)?,
            _ => self.transform_response_native(model, response)?,
        };
        // Captured against this server's target, not the OpenAI adapter's, so
        // the block's origin names the provider that actually issued it.
        crate::derived_response::capture_unary(&self.replay_target(model), &native, &mut result)?;
        Ok(result)
    }

    fn transform_stream_chunk(&self, model: &str, chunk: &str) -> Result<Option<String>> {
        crate::json_bounds::enforce_sse_complexity(chunk)?;
        let result = match self.wire {
            WireFormat::OpenAiResponses => self
                .responses_adapter()
                .transform_stream_chunk_native(model, chunk)?,
            _ => self.transform_stream_chunk_native(model, chunk)?,
        };
        let native: Value = match serde_json::from_str(chunk) {
            Ok(v) => v,
            Err(_) => return Ok(result),
        };
        crate::reasoning::capture_stream(&self.replay_target(model), &native, result)
    }
}

impl OpenAiCompatible {
    fn transform_response_native(&self, _model: &str, mut response: Value) -> Result<Value> {
        if !response.is_object() {
            return Err(ShimError::ProviderError {
                status: 502,
                body: "invalid upstream response shape".into(),
                retry_after: None,
            });
        }
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
                    retry_after: None,
                });
            }
        }

        crate::usage::normalize_response(&mut response);
        Ok(response)
    }
}

impl OpenAiCompatible {
    fn transform_stream_chunk_native(&self, _model: &str, chunk: &str) -> Result<Option<String>> {
        let mut parsed: Value = match serde_json::from_str(chunk) {
            Ok(v) => v,
            Err(_) => return Ok(None),
        };

        if parsed.get("usage").is_some_and(Value::is_object) {
            crate::usage::normalize_response(&mut parsed);
        }
        Ok(Some(serde_json::to_string(&parsed)?))
    }
}
