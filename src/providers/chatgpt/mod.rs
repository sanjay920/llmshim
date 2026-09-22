//! ChatGPT subscription provider. Address models explicitly as `chatgpt/<model>`.
mod auth;
mod streaming;

pub use auth::{ChatGptAuth, DeviceCode, LoginStatus};
pub(crate) use streaming::collect_response_with_terminal;

pub(crate) fn transform_collected_response(
    target: &crate::reasoning::ReplayTarget,
    model: &str,
    response: Value,
) -> Result<Value> {
    let native = response.clone();
    let mut transformed = transform_response(model, response)?;
    crate::derived_response::capture_unary(target, &native, &mut transformed)?;
    Ok(transformed)
}
pub(crate) use streaming::transform_chunk as parse_stream_chunk;

use crate::{
    error::Result,
    provider::{Provider, ProviderRequest},
    providers::openai::OpenAi,
};
use auth::{auth_error, Tokens};
use serde_json::{json, Value};
use std::{future::Future, pin::Pin};

pub struct ChatGpt {
    pub auth: ChatGptAuth,
    pub base_url: String,
    originator: String,
    user_agent: String,
}

fn env_value(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.trim().is_empty())
}

fn validate_model(model: &str) -> Result<()> {
    if crate::models::CHATGPT_MODELS
        .iter()
        .any(|entry| entry.name == model)
    {
        Ok(())
    } else {
        Err(auth_error(400, "unsupported model; use chatgpt/gpt-6-astra, chatgpt/gpt-5.6-sol, chatgpt/gpt-5.6-terra, or chatgpt/gpt-5.6-luna"))
    }
}

pub(super) fn translator() -> OpenAi {
    OpenAi::new(String::new())
}

impl Default for ChatGpt {
    fn default() -> Self {
        Self::new(ChatGptAuth::from_env())
    }
}

impl ChatGpt {
    pub fn new(auth: ChatGptAuth) -> Self {
        let originator = env_value("CHATGPT_ORIGINATOR").unwrap_or_else(|| "codex_cli_rs".into());
        let mut user_agent = env_value("CHATGPT_USER_AGENT").unwrap_or_else(|| {
            format!(
                "{}/{} ({}; {}) llmshim",
                originator,
                env!("CARGO_PKG_VERSION"),
                std::env::consts::OS,
                std::env::consts::ARCH
            )
        });
        if let Some(suffix) = env_value("CHATGPT_USER_AGENT_SUFFIX") {
            user_agent.push_str(&format!(" {suffix}"));
        }
        Self {
            auth,
            base_url: env_value("CHATGPT_API_BASE")
                .or_else(|| env_value("OPENAI_CHATGPT_API_BASE"))
                .unwrap_or_else(|| "https://chatgpt.com/backend-api/codex".into()),
            originator,
            user_agent,
        }
    }

    pub fn with_base_url(mut self, base_url: String) -> Self {
        self.base_url = base_url;
        self
    }

    fn request(&self, model: &str, request: &Value, tokens: Tokens) -> Result<ProviderRequest> {
        // Isolate ChatGPT's native namespace from API-key OpenAI extensions.
        let mut schema_budget = crate::schema::RequestBudget::new();
        schema_budget.reserve_request_schemas(request)?;
        let mut input = request.clone();
        crate::reasoning::sanitize_extensions(&mut input);
        let obj = input
            .as_object_mut()
            .ok_or(crate::error::ShimError::MissingModel)?;
        obj.remove("x-openai");
        // The shared Responses translator accepts system/developer strings.
        // Preserve text-block instructions too, instead of silently losing them.
        if let Some(messages) = obj.get_mut("messages").and_then(Value::as_array_mut) {
            for message in messages {
                if matches!(message["role"].as_str(), Some("system" | "developer")) {
                    if let Some(blocks) = message["content"].as_array() {
                        message["content"] = json!(blocks
                            .iter()
                            .filter_map(|b| b["text"].as_str())
                            .collect::<Vec<_>>()
                            .join("\n"));
                    }
                }
            }
        }
        let target = crate::reasoning::ReplayTarget::new(
            "chatgpt",
            model,
            crate::reasoning::WireFormat::OpenAiResponses,
        )
        .bind_account(&self.base_url, tokens.account_id.as_deref());
        let mut req = translator().transform_request_for_target_with_budget(
            model,
            &input,
            &target,
            &mut schema_budget,
        )?;
        let body = req.body.as_object_mut().unwrap();
        if let Some(ext) = request.get("x-chatgpt").and_then(Value::as_object) {
            body.extend(ext.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        // Mirrors LiteLLM's subscription backend allowlist. In particular token
        // limits, sampling parameters and metadata are rejected upstream.
        body.retain(|k, _| {
            matches!(
                k.as_str(),
                "model"
                    | "input"
                    | "instructions"
                    | "stream"
                    | "store"
                    | "include"
                    | "text"
                    | "tools"
                    | "tool_choice"
                    | "reasoning"
                    | "previous_response_id"
                    | "truncation"
                    | "prompt_cache_key"
                    | "prompt_cache_retention"
            )
        });
        body.insert("model".into(), json!(model));
        body.insert("stream".into(), json!(true));
        body.insert("store".into(), json!(false));
        body.entry("instructions")
            .or_insert(json!("You are a helpful assistant."));
        if !body["instructions"].is_string() {
            return Err(auth_error(400, "instructions must be a string"));
        }
        let include = body
            .entry("include")
            .or_insert(json!([]))
            .as_array_mut()
            .ok_or_else(|| auth_error(400, "include must be an array"))?;
        if !include.iter().any(|v| v == "reasoning.encrypted_content") {
            include.push(json!("reasoning.encrypted_content"));
        }
        if let Some(choice) = body.get_mut("tool_choice") {
            if choice["type"] == "function" && choice["function"]["name"].is_string() {
                *choice = json!({"type": "function", "name": choice["function"]["name"]});
            }
        }
        crate::schema::normalize_native_tools(
            crate::schema::Target::OpenAiResponses,
            &mut req.body,
            &mut schema_budget,
        )?;
        crate::shim::native_format(
            request,
            crate::reasoning::WireFormat::OpenAiResponses,
            &mut req.body,
            &mut schema_budget,
        )?;
        crate::cache::finish_request(
            request,
            &mut req.body,
            crate::reasoning::WireFormat::OpenAiResponses,
        )?;
        req.url = format!("{}/responses", self.base_url.trim_end_matches('/'));
        crate::reasoning::enforce_stateless(&mut req.body)?;
        crate::toolcall::validate_native(&req.body, &target)?;
        req.headers = vec![
            (
                "Authorization".into(),
                format!("Bearer {}", tokens.access_token),
            ),
            ("Content-Type".into(), "application/json".into()),
            ("Accept".into(), "text/event-stream".into()),
            ("originator".into(), self.originator.clone()),
            ("User-Agent".into(), self.user_agent.clone()),
        ];
        if let Some(id) = tokens.account_id.filter(|s| !s.is_empty()) {
            req.headers.push(("ChatGPT-Account-Id".into(), id));
        }
        Ok(req)
    }
}

impl Provider for ChatGpt {
    fn name(&self) -> &str {
        "chatgpt"
    }

    fn request_admission_policy(&self) -> crate::provider::RequestAdmissionPolicy {
        crate::provider::RequestAdmissionPolicy::namespaced(
            "x-chatgpt",
            &["input"],
            &["instructions", "text", "tools", "tool_choice", "reasoning"],
            &[],
        )
    }

    fn replay_target(&self, model: &str) -> crate::reasoning::ReplayTarget {
        crate::reasoning::ReplayTarget::new(
            "chatgpt",
            model,
            crate::reasoning::WireFormat::OpenAiResponses,
        )
        .bind_account(&self.base_url, self.auth.replay_account().as_deref())
    }

    fn request_replay_target(
        &self,
        model: &str,
        request: &ProviderRequest,
    ) -> crate::reasoning::ReplayTarget {
        let account = request
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("chatgpt-account-id"))
            .map(|(_, v)| v.as_str());
        crate::reasoning::ReplayTarget::new(
            "chatgpt",
            model,
            crate::reasoning::WireFormat::OpenAiResponses,
        )
        .bind_account(&self.base_url, account)
    }

    fn transform_request(&self, model: &str, request: &Value) -> Result<ProviderRequest> {
        validate_model(model)?;
        self.request(model, request, self.auth.cached_credentials()?)
    }

    fn prepare_request<'a>(
        &'a self,
        model: &'a str,
        request: &'a Value,
    ) -> Pin<Box<dyn Future<Output = Result<ProviderRequest>> + Send + 'a>> {
        Box::pin(async move {
            validate_model(model)?;
            self.request(model, request, self.auth.credentials().await?)
        })
    }

    fn transform_response(&self, model: &str, response: Value) -> Result<Value> {
        let native = response.clone();
        let mut result = transform_response(model, response)?;
        crate::derived_response::capture_unary(&self.replay_target(model), &native, &mut result)?;
        Ok(result)
    }

    fn transform_stream_chunk(&self, model: &str, chunk: &str) -> Result<Option<String>> {
        let result = streaming::transform_chunk(model, chunk)?;
        let Ok(native) = serde_json::from_str(chunk) else {
            return Ok(result);
        };
        crate::reasoning::capture_stream(&self.replay_target(model), &native, result)
    }
}

pub(super) fn transform_response(model: &str, response: Value) -> Result<Value> {
    let mut result = translator().transform_response(model, response.clone())?;
    // ChatGPT and API-key OpenAI share bare model names. Preserve the route so
    // proxy/gateway responses and fallback attribution cannot select OpenAI.
    result["model"] = json!(format!("chatgpt/{model}"));
    let mut text = String::new();
    let mut has_text = false;
    for item in response["output"].as_array().into_iter().flatten() {
        if item["type"] == "message" {
            for part in item["content"].as_array().into_iter().flatten() {
                if let Some(t) = part["text"].as_str() {
                    has_text = true;
                    text.push_str(t);
                }
            }
        }
    }
    if has_text {
        result["choices"][0]["message"]["content"] = json!(text);
    }
    if response["status"] == "completed" && result["choices"][0]["message"]["tool_calls"].is_array()
    {
        result["choices"][0]["finish_reason"] = json!("tool_calls");
    }
    Ok(result)
}
