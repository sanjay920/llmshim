//! Native inbound API facades over the existing proxy/gateway handlers.
mod receipts;
use axum::{
    body::{to_bytes, Body},
    extract::Request,
    http::{header, StatusCode},
    middleware::Next,
    response::{
        sse::{Event, Sse},
        IntoResponse, Response,
    },
    Json,
};
use futures::StreamExt;
pub use receipts::Receipts;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    convert::Infallible,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, OnceLock,
    },
};
use tokio::sync::Semaphore;

const RECEIPT_WORKERS: usize = 2;
const RECEIPT_INGRESS_CAPACITY: usize = 8;
const RECEIPT_EGRESS_CAPACITY: usize = 4;
const RECEIPT_QUEUE_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Clone, Debug)]
pub(crate) struct DefaultReceiptStore {
    receipts: Arc<Receipts>,
    executor: ReceiptExecutor,
}

impl DefaultReceiptStore {
    pub(crate) fn from_env() -> Self {
        Self::new(Arc::new(Receipts::from_env()))
    }

    pub(crate) fn new(receipts: Arc<Receipts>) -> Self {
        Self {
            receipts,
            executor: ReceiptExecutor::new(),
        }
    }
}

pub(crate) async fn install_default_receipt_store(
    axum::extract::State(default_store): axum::extract::State<DefaultReceiptStore>,
    mut request: Request,
    next: Next,
) -> Response {
    if request.extensions().get::<DefaultReceiptStore>().is_none() {
        request.extensions_mut().insert(default_store);
    }
    next.run(request).await
}

pub(crate) async fn bound_inference_request_json(request: Request, next: Next) -> Response {
    if request.method() != axum::http::Method::POST
        || !matches!(
            request.uri().path(),
            "/v1/chat" | "/v1/chat/stream" | "/v1/chat/completions" | "/v1/messages"
        )
    {
        return next.run(request).await;
    }
    let path = request.uri().path().to_owned();
    let (parts, body) = request.into_parts();
    let bytes = match to_bytes(body, 2 * 1024 * 1024).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return inbound_json_failure(
                &path,
                StatusCode::PAYLOAD_TOO_LARGE,
                "request exceeds size limit",
            )
        }
    };
    match crate::json_bounds::parse_slice(&bytes, crate::json_bounds::Limits::INBOUND) {
        Ok(_) => {
            next.run(Request::from_parts(parts, Body::from(bytes)))
                .await
        }
        Err(crate::json_bounds::ParseError::Malformed(_)) => {
            inbound_json_failure(&path, StatusCode::BAD_REQUEST, "invalid JSON request")
        }
        Err(crate::json_bounds::ParseError::Complexity) => inbound_json_failure(
            &path,
            StatusCode::PAYLOAD_TOO_LARGE,
            "request JSON exceeds complexity limit",
        ),
    }
}

fn inbound_json_failure(path: &str, status: StatusCode, message: &str) -> Response {
    match path {
        "/v1/chat/completions" => fail(Wire::Chat, status, message),
        "/v1/messages" => fail(Wire::Messages, status, message),
        _ => (
            status,
            Json(json!({"error":{"code": if status == StatusCode::PAYLOAD_TOO_LARGE {"request_too_large"} else {"invalid_request"},"message":message}})),
        )
            .into_response(),
    }
}

fn fallback_receipt_store() -> DefaultReceiptStore {
    static STORE: OnceLock<DefaultReceiptStore> = OnceLock::new();
    STORE.get_or_init(DefaultReceiptStore::from_env).clone()
}

#[derive(Clone, Debug)]
struct ReceiptExecutor {
    workers: Arc<Semaphore>,
    ingress_worker: Arc<Semaphore>,
    ingress_capacity: Arc<Semaphore>,
    egress_capacity: Arc<Semaphore>,
    lock_timeout: std::time::Duration,
}

#[derive(Clone, Copy)]
enum ReceiptWorkKind {
    Ingress,
    Egress,
}

struct ReceiptWorkPermits {
    _capacity: tokio::sync::OwnedSemaphorePermit,
    _ingress_worker: Option<tokio::sync::OwnedSemaphorePermit>,
    _worker: tokio::sync::OwnedSemaphorePermit,
}

#[derive(Debug)]
enum ReceiptWorkError {
    Busy,
    Failed(String),
}

struct CancelReceiptWork {
    cancelled: Arc<AtomicBool>,
    armed: bool,
}

impl Drop for CancelReceiptWork {
    fn drop(&mut self) {
        if self.armed {
            self.cancelled.store(true, Ordering::Release);
        }
    }
}

impl ReceiptExecutor {
    fn new() -> Self {
        Self {
            workers: Arc::new(Semaphore::new(RECEIPT_WORKERS)),
            ingress_worker: Arc::new(Semaphore::new(1)),
            ingress_capacity: Arc::new(Semaphore::new(RECEIPT_INGRESS_CAPACITY)),
            egress_capacity: Arc::new(Semaphore::new(RECEIPT_EGRESS_CAPACITY)),
            lock_timeout: receipts::configured_http_lock_timeout(),
        }
    }

    async fn acquire(
        &self,
        kind: ReceiptWorkKind,
    ) -> std::result::Result<ReceiptWorkPermits, ReceiptWorkError> {
        let capacity = match kind {
            ReceiptWorkKind::Ingress => self.ingress_capacity.clone(),
            ReceiptWorkKind::Egress => self.egress_capacity.clone(),
        }
        .try_acquire_owned()
        .map_err(|_| ReceiptWorkError::Busy)?;
        let acquire = async {
            let ingress_worker = match kind {
                ReceiptWorkKind::Ingress => Some(
                    self.ingress_worker
                        .clone()
                        .acquire_owned()
                        .await
                        .map_err(|_| ReceiptWorkError::Busy)?,
                ),
                ReceiptWorkKind::Egress => None,
            };
            let worker = self
                .workers
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| ReceiptWorkError::Busy)?;
            Ok(ReceiptWorkPermits {
                _capacity: capacity,
                _ingress_worker: ingress_worker,
                _worker: worker,
            })
        };
        tokio::time::timeout(RECEIPT_QUEUE_WAIT, acquire)
            .await
            .map_err(|_| ReceiptWorkError::Busy)?
    }

    async fn run<T, F>(
        &self,
        kind: ReceiptWorkKind,
        operation: F,
    ) -> std::result::Result<T, ReceiptWorkError>
    where
        T: Send + 'static,
        F: FnOnce() -> Result<T> + Send + 'static,
    {
        let permits = self.acquire(kind).await?;
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = cancelled.clone();
        let lock_timeout = self.lock_timeout;
        let mut cancellation = CancelReceiptWork {
            cancelled,
            armed: true,
        };
        let worker = tokio::task::spawn_blocking(move || {
            let _permits = permits;
            if worker_cancelled.load(Ordering::Acquire) {
                return Err(ReceiptWorkError::Busy);
            }
            receipts::with_http_lock_timeout(lock_timeout, operation).map_err(|message| {
                if message == receipts::BUSY_ERROR_MESSAGE {
                    ReceiptWorkError::Busy
                } else {
                    ReceiptWorkError::Failed(message)
                }
            })
        });
        let result = worker.await.map_err(|_| {
            ReceiptWorkError::Failed("native replay metadata is unavailable".into())
        })?;
        cancellation.armed = false;
        result
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wire {
    Chat,
    Messages,
}
type Result<T> = std::result::Result<T, String>;
fn array<'a>(value: &'a Value, name: &str) -> Result<&'a Vec<Value>> {
    value
        .as_array()
        .ok_or_else(|| format!("{name} must be an array"))
}
fn native_call(call: &Value) -> Value {
    json!({"id":call["id"],"type":"function","function":{"name":call["function"]["name"],"arguments":call["function"]["arguments"]}})
}
fn call_content(call: &Value) -> Result<Value> {
    let args = call["function"]["arguments"]
        .as_str()
        .ok_or("tool arguments must be JSON text")?;
    let value: Value = serde_json::from_str(args).map_err(|_| "invalid tool argument JSON")?;
    Ok(json!({"id":call["id"],"name":call["function"]["name"],"arguments":value}))
}
fn import_call(call: &Value, receipts: &Receipts, scope: &str) -> Result<Value> {
    if let Some(issued) = receipts.get(scope, "call", &call["id"])? {
        if call_content(&issued)? != call_content(call)? {
            return Err("issued tool call was modified".into());
        }
        return Ok(issued);
    }
    // Legacy client-created ids remain readable. Owned ids need their receipt.
    if call["id"]
        .as_str()
        .is_some_and(|id| id.starts_with("call_ls_"))
    {
        return Err("owned tool call is missing its replay receipt".into());
    }
    Ok(native_call(call))
}
/// An OpenAI-shaped `unsupported_parameter` refusal, carried through
/// `normalize_error` so the client sees `param` and `code`, not a bare string.
fn unsupported_parameter(param: &str, message: &str) -> String {
    json!({"error":{
        "message": message,
        "type": "invalid_request_error",
        "param": param,
        "code": "unsupported_parameter",
    }})
    .to_string()
}

fn message_key(message: &Value) -> Value {
    json!({"content":message["content"],"reasoning_content":message["reasoning_content"],"reasoning":message["reasoning"],"reasoning_details":message["reasoning_details"],"thinking_blocks":message["thinking_blocks"]})
}

pub fn request_to_chat(
    native: &Value,
    wire: Wire,
    receipts: &Receipts,
    scope: &str,
) -> Result<Value> {
    let obj = native.as_object().ok_or("request must be an object")?;
    let model = native["model"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or("model is required")?;
    if native.get("n").is_some_and(|n| n != 1) {
        // Rejected rather than emulated: llmshim's OpenAI backend is the
        // Responses API, which has no `n`, and neither do Anthropic Messages or
        // Gemini generateContent. Emulation would mean N fan-out requests whose
        // cost, rate-limit footprint and cache behaviour all differ from what
        // the caller asked for, and the single-message proxy projects choice
        // zero regardless. A correctly shaped refusal is more useful than a
        // silently different execution model.
        return Err(unsupported_parameter(
            "n",
            "Unsupported value: 'n' must be 1. This endpoint returns one completion per request.",
        ));
    }
    if let Some(tools) = native.get("tools") {
        for tool in array(tools, "tools")? {
            let supported = match wire {
                Wire::Chat => tool["type"] == "function",
                Wire::Messages => tool.get("type").is_none() || tool["type"] == "custom",
            };
            if !supported {
                return Err("this endpoint supports custom function tools".into());
            }
        }
    }
    let mut messages = Vec::new();
    let mut boundaries = Vec::new();
    if wire == Wire::Messages {
        if let Some(system) = native.get("system") {
            messages.push(json!({"role":"system","content":system}));
        }
    }
    for message in array(&native["messages"], "messages")? {
        let role = message["role"].as_str().ok_or("message role is required")?;
        if wire == Wire::Chat {
            let mut canonical = message.clone();
            for field in [
                "reasoning",
                "reasoning_content",
                "reasoning_details",
                "thinking_blocks",
                "reasoning_signature",
                "redacted_reasoning_content",
                "reasoning_origin",
            ] {
                canonical.as_object_mut().unwrap().remove(field);
            }
            if role == "assistant" {
                if let Some(reasoning) = receipts.get(scope, "reasoning", &message_key(message))? {
                    canonical["reasoning"] = reasoning;
                }
                if let Some(calls) = message.get("tool_calls") {
                    canonical["tool_calls"] = json!(array(calls, "tool_calls")?
                        .iter()
                        .map(|call| import_call(call, receipts, scope))
                        .collect::<Result<Vec<_>>>()?);
                }
            }
            messages.push(canonical);
            boundaries.push(messages.len().checked_sub(1));
            continue;
        }
        if !matches!(role, "user" | "assistant") {
            return Err("Messages roles must be user or assistant".into());
        }
        if message["content"].is_string() {
            messages.push(message.clone());
            boundaries.push(messages.len().checked_sub(1));
            continue;
        }
        let mut content = Vec::new();
        let mut calls = Vec::new();
        let mut reasoning = Vec::new();
        for block in array(&message["content"], "message content")? {
            match block["type"].as_str() {
                Some("tool_use") if role == "assistant" => {
                    let call = json!({"id":block["id"],"type":"function","function":{"name":block["name"],"arguments":block["input"].to_string()}});
                    let mut call = import_call(&call, receipts, scope)?;
                    if let Some(cache) = block.get("cache_control") {
                        call["cache_control"] = cache.clone();
                    }
                    calls.push(call);
                }
                Some("tool_result") if role == "user" => {
                    let mut result = json!({"role":"tool","tool_call_id":block["tool_use_id"],"content":block["content"]});
                    for field in ["is_error", "cache_control"] {
                        if let Some(value) = block.get(field) {
                            result[field] = value.clone();
                        }
                    }
                    if result.get("is_error").is_some_and(|v| !v.is_boolean()) {
                        return Err("is_error must be a boolean".into());
                    }
                    messages.push(result);
                }
                Some("thinking" | "redacted_thinking") if role == "assistant" => {
                    if let Some(original) = receipts.get(scope, "block", block)? {
                        reasoning.push(original);
                    }
                }
                Some("tool_use" | "tool_result" | "thinking" | "redacted_thinking") => {
                    return Err("content block is not valid for this role".into())
                }
                _ => content.push(block.clone()),
            }
        }
        if !content.is_empty() || !calls.is_empty() || !reasoning.is_empty() {
            let mut canonical = json!({"role":role,"content":content});
            if !calls.is_empty() {
                canonical["tool_calls"] = json!(calls);
            }
            if !reasoning.is_empty() {
                canonical["reasoning"] = json!(reasoning);
            }
            messages.push(canonical);
        }
        boundaries.push(messages.len().checked_sub(1));
    }
    let mut config = obj.clone();
    for key in ["model", "messages", "system", "stream", "n"] {
        config.remove(key);
    }
    if let Some(segments) = config
        .get_mut("x-cache")
        .and_then(|cache| cache.get_mut("segments"))
        .and_then(Value::as_array_mut)
    {
        for segment in segments {
            let index = segment["upto_message"]
                .as_u64()
                .and_then(|n| usize::try_from(n).ok())
                .and_then(|n| boundaries.get(n))
                .and_then(|index| *index)
                .ok_or("cache boundary does not identify a message")?;
            segment["upto_message"] = json!(index);
        }
    }
    if wire == Wire::Messages {
        if let Some(stop) = config.remove("stop_sequences") {
            config.insert("stop".into(), stop);
        }
        if let Some(format) = native
            .pointer("/output_config/format")
            .or_else(|| native.get("output_format"))
        {
            if format["type"] == "json_schema" {
                config.insert(
                    "response_format".into(),
                    json!({"type":"json_schema","json_schema":{"schema":format["schema"]}}),
                );
            }
        }
        if let Some(tools) = native.get("tools") {
            config.insert("tools".into(),json!(array(tools,"tools")?.iter().map(|tool|{
                let mut out=json!({"type":"function","function":{"name":tool["name"],"parameters":tool["input_schema"]}});
                for field in ["description","strict"] {if let Some(value)=tool.get(field){out["function"][field]=value.clone();}}
                if let Some(cache)=tool.get("cache_control"){out["cache_control"]=cache.clone();}out
            }).collect::<Vec<_>>()));
        }
        if let Some(choice) = native.get("tool_choice") {
            let choice = match choice["type"].as_str() {
                Some("any") => json!("required"),
                Some("auto") => json!("auto"),
                Some("none") => json!("none"),
                Some("tool") => json!({"type":"function","function":{"name":choice["name"]}}),
                _ => return Err("invalid tool_choice".into()),
            };
            config.insert("tool_choice".into(), choice);
            if native["tool_choice"]["disable_parallel_tool_use"] == true {
                config.insert("parallel_tool_calls".into(), json!(false));
            }
        }
    }
    Ok(
        json!({"model":model,"messages":messages,"stream":native["stream"].as_bool().unwrap_or(false),"fallback":native["fallback"],"provider_config":config}),
    )
}

pub fn response_from_chat(
    response: &Value,
    wire: Wire,
    receipts: &Receipts,
    scope: &str,
) -> Result<Value> {
    let message = &response["message"];
    let mut usage = response["usage"].clone();
    if !usage.is_object() {
        usage = json!({});
    }
    for key in [
        "input_tokens",
        "output_tokens",
        "total_tokens",
        "cache_read_tokens",
        "cache_write_tokens",
    ] {
        if !usage[key].is_u64() {
            usage[key] = json!(0);
        }
    }
    let calls = message["tool_calls"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    for call in &calls {
        receipts.put(scope, "call", &call["id"], call)?;
    }
    let finish = response["finish_reason"]
        .as_str()
        .unwrap_or(if !calls.is_empty() {
            "tool_calls"
        } else {
            "stop"
        });
    let mut out = if wire == Wire::Chat {
        let mut exported = json!({"role":"assistant","content":message["content"]});
        if let Some(refusal) = message.get("refusal") {
            exported["refusal"] = refusal.clone();
        }
        if !calls.is_empty() {
            exported["tool_calls"] = json!(calls.iter().map(native_call).collect::<Vec<_>>());
        }
        if let Some(reasoning) = message.get("reasoning").filter(|r| r.is_array()) {
            exported["reasoning"] = reasoning.clone();
            exported["reasoning_content"] = json!(crate::reasoning::reasoning_text(message));
            receipts.put(scope, "reasoning", &message_key(&exported), reasoning)?;
        }
        json!({"id":response["id"],"object":"chat.completion","created":response.get("created").cloned().unwrap_or(json!(chrono::Utc::now().timestamp())),"model":response["model"],"choices":[{"index":0,"message":exported,"finish_reason":finish}],"usage":{"prompt_tokens":usage["input_tokens"],"completion_tokens":usage["output_tokens"],"total_tokens":usage["total_tokens"],"cache_read_tokens":usage["cache_read_tokens"],"cache_write_tokens":usage["cache_write_tokens"],"cost_usd":usage["cost_usd"],"cost_source":usage["cost_source"],"prompt_tokens_details":{"cached_tokens":usage["cache_read_tokens"]}}})
    } else {
        let mut content = Vec::new();
        for block in message["reasoning"].as_array().into_iter().flatten() {
            let same_wire = block["origin"]["wire"] == "anthropic-messages";
            let handle = format!("lsr_{:x}", Sha256::digest(block.to_string().as_bytes()));
            let exported = if block["kind"] == "text" {
                json!({"type":"thinking","thinking":block["text"].as_str().unwrap_or(""),"signature":if same_wire{block["signature"].as_str().unwrap_or(&handle)}else{&handle}})
            } else {
                json!({"type":"redacted_thinking","data":if same_wire{block["data"].as_str().unwrap_or(&handle)}else{&handle}})
            };
            receipts.put(scope, "block", &exported, block)?;
            content.push(exported);
        }
        if let Some(text) = message["content"].as_str().filter(|s| !s.is_empty()) {
            content.push(json!({"type":"text","text":text}));
        } else if let Some(blocks) = message["content"].as_array() {
            content.extend(blocks.iter().cloned());
        }
        if let Some(refusal) = message["refusal"].as_str() {
            content.push(json!({"type":"text","text":refusal}));
        }
        for call in calls {
            let parsed = call_content(&call)?;
            content.push(json!({"type":"tool_use","id":parsed["id"],"name":parsed["name"],"input":parsed["arguments"]}));
        }
        json!({"id":response["id"],"type":"message","role":"assistant","model":response["model"],"content":content,"stop_reason":match finish{"tool_calls"=>"tool_use","length"=>"max_tokens","content_filter"=>"refusal",_=>"end_turn"},"stop_sequence":null,"usage":{"input_tokens":usage["input_tokens"],"output_tokens":usage["output_tokens"],"cache_read_input_tokens":usage["cache_read_tokens"],"cache_creation_input_tokens":usage["cache_write_tokens"],"cost_usd":usage["cost_usd"],"cost_source":usage["cost_source"]}})
    };
    if let Some(served) = response.get("x-llmshim-served-model") {
        out["x-llmshim-served-model"] = served.clone();
    }
    Ok(out)
}

fn error_body(wire: Wire, message: &str) -> Value {
    native_error_body(wire, message, "invalid_request_error")
}

fn native_error_body(wire: Wire, message: &str, fallback_type: &str) -> Value {
    render_error(wire, &crate::error::normalize_error(message), fallback_type)
}

fn render_error(wire: Wire, error: &crate::error::NormalizedError, fallback_type: &str) -> Value {
    let kind = match wire {
        Wire::Messages => error.code_type().or(error.kind.as_deref()),
        Wire::Chat => error.kind.as_deref().or(error.code_type()),
    }
    .unwrap_or(fallback_type);
    match wire {
        Wire::Messages => {
            let kind = if kind == "server_error" {
                "api_error"
            } else {
                kind
            };
            json!({"type":"error","error":{"type":kind,"message":error.message}})
        }
        Wire::Chat => {
            json!({"error":{"type":kind,"message":error.message,"param":error.param,"code":error.code}})
        }
    }
}

fn error_from_event(wire: Wire, event: &Value) -> Value {
    match event.get("error").filter(|error| error.is_object()) {
        Some(error) => error_body(wire, &json!({"error":error}).to_string()),
        None => error_body(
            wire,
            event["message"]
                .as_str()
                .unwrap_or("upstream stream failed"),
        ),
    }
}

pub(crate) fn fail(wire: Wire, status: StatusCode, message: &str) -> Response {
    (status, Json(error_for_status(wire, status, message))).into_response()
}

fn receipt_work_failure(
    wire: Wire,
    error: ReceiptWorkError,
    conversion_status: StatusCode,
) -> Response {
    match error {
        ReceiptWorkError::Failed(message) => fail(wire, conversion_status, &message),
        ReceiptWorkError::Busy => {
            let mut response = fail(
                wire,
                StatusCode::SERVICE_UNAVAILABLE,
                "native replay metadata is busy; retry the request",
            );
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, header::HeaderValue::from_static("1"));
            response
        }
    }
}
fn error_for_status(wire: Wire, status: StatusCode, message: &str) -> Value {
    native_error_body(wire, message, fallback_error_type(wire, status))
}
fn fallback_error_type(wire: Wire, status: StatusCode) -> &'static str {
    match status.as_u16() {
        401 => "authentication_error",
        403 => "permission_error",
        429 => "rate_limit_error",
        503 if wire == Wire::Messages => "overloaded_error",
        500..=599 => "api_error",
        _ => "invalid_request_error",
    }
}

/// Both aliases call the existing chat handlers after this body translation, so
/// queueing, quotas, authentication, retry headers and cancellation stay shared.
pub async fn translate(request: Request, next: Next) -> Response {
    if request.method() != axum::http::Method::POST {
        return next.run(request).await;
    }
    let wire = match request.uri().path() {
        "/v1/chat/completions" => Wire::Chat,
        "/v1/messages" => Wire::Messages,
        _ => return next.run(request).await,
    };
    let default_store = request
        .extensions()
        .get::<DefaultReceiptStore>()
        .cloned()
        .unwrap_or_else(fallback_receipt_store);
    let receipts = request
        .extensions()
        .get::<Arc<Receipts>>()
        .cloned()
        .unwrap_or_else(|| default_store.receipts.clone());
    let receipt_executor = default_store.executor;
    let (mut parts, body) = request.into_parts();
    normalize_auth(&mut parts.headers);
    let identity = parts
        .headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("anonymous");
    let identity = identity
        .strip_prefix("Bearer ")
        .or_else(|| identity.strip_prefix("bearer "))
        .unwrap_or(identity)
        .trim();
    let scope = format!("{:x}", Sha256::digest(identity.as_bytes()));
    if let Some(key) = parts.headers.get("idempotency-key") {
        let scoped = format!(
            "native:{:?}:{}:{}",
            wire,
            scope,
            String::from_utf8_lossy(key.as_bytes())
        );
        let key = format!("{:x}", Sha256::digest(scoped.as_bytes()));
        if let Ok(header) = key.parse() {
            parts.headers.insert("idempotency-key", header);
        }
    }
    let bytes = match to_bytes(body, 2 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => {
            return fail(
                wire,
                StatusCode::PAYLOAD_TOO_LARGE,
                "request exceeds size limit",
            )
        }
    };
    let native: Value =
        match crate::json_bounds::parse_slice(&bytes, crate::json_bounds::Limits::INBOUND) {
            Ok(value) => value,
            Err(crate::json_bounds::ParseError::Malformed(_)) => {
                return fail(wire, StatusCode::BAD_REQUEST, "invalid JSON request")
            }
            Err(crate::json_bounds::ParseError::Complexity) => {
                return fail(
                    wire,
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "request JSON exceeds complexity limit",
                )
            }
        };
    let response_model = native["model"].clone();
    let request_receipts = receipts.clone();
    let request_scope = scope.clone();
    let chat = match receipt_executor
        .run(ReceiptWorkKind::Ingress, move || {
            request_to_chat(&native, wire, &request_receipts, &request_scope)
        })
        .await
    {
        Ok(value) => value,
        Err(error) => return receipt_work_failure(wire, error, StatusCode::BAD_REQUEST),
    };
    parts.headers.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/json"),
    );
    parts.headers.remove(header::CONTENT_LENGTH);
    let response = next
        .run(Request::from_parts(parts, Body::from(chat.to_string())))
        .await;
    let (mut parts, body) = response.into_parts();
    parts.headers.remove(header::CONTENT_LENGTH);
    if parts.status.is_success()
        && parts
            .headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("text/event-stream"))
    {
        let model = response_model;
        let events = async_stream::stream! {
            let mut stream = Box::pin(crate::sse::data(body.into_data_stream()));
            let mut response=json!({"id":format!("msg_{}",uuid::Uuid::new_v4().simple()),"model":model,"message":{"role":"assistant","content":""},"usage":{},"finish_reason":"stop"});
            response["created"]=json!(chrono::Utc::now().timestamp());
            let mut text_started=false;
            let start=if wire==Wire::Chat {
                json!({"id":response["id"],"model":model,"object":"chat.completion.chunk","created":response["created"],"choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]})
            } else {json!({"type":"message_start","message":{"id":response["id"],"type":"message","role":"assistant","model":model,"content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":0,"output_tokens":0}}})};
            let mut start_event = Event::default();
            if wire == Wire::Messages {
                start_event = start_event.event("message_start");
            }
            yield Ok::<Event, Infallible>(start_event.data(start.to_string()));
            let mut reasoning=crate::reasoning::ReasoningAccumulator::default();let mut size=0usize;let mut done=false;
            while let Some(event)=stream.next().await {
                let event=match event{Ok(event)=>event,Err(_)=>{yield Ok::<Event,Infallible>(Event::default().event("error").data(error_body(wire,"upstream stream failed").to_string()));return;}};
                size=size.saturating_add(event.len());
                if size>32*1024*1024 {yield Ok(Event::default().event("error").data(error_body(wire,"response exceeds size limit").to_string()));return;}
                let data:Value=match serde_json::from_str(&event){Ok(data)=>data,Err(_)=>continue};
                match data["type"].as_str() {
                    Some("content")=>{
                        crate::streaming::append_string_fragment(
                            &mut response["message"]["content"],
                            data["text"].as_str().unwrap_or(""),
                        );
                        if wire==Wire::Chat {
                            yield Ok(Event::default().data(json!({"id":response["id"],"model":model,"object":"chat.completion.chunk","created":response["created"],"choices":[{"index":0,"delta":{"content":data["text"]},"finish_reason":null}]}).to_string()));
                        } else {
                            if !text_started {yield Ok(Event::default().event("content_block_start").data(json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}).to_string()));text_started=true;}
                            yield Ok(Event::default().event("content_block_delta").data(json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":data["text"]}}).to_string()));
                        }
                    },
                    Some("reasoning")=>if reasoning.push(&json!({"reasoning":data["blocks"]})).is_err(){yield Ok(Event::default().event("error").data(error_body(wire,crate::stream_retention::RETENTION_ERROR).to_string()));return;},
                    Some("tool_call")=>{if !response["message"]["tool_calls"].is_array(){response["message"]["tool_calls"]=json!([]);}
                        let mut call=json!({"id":data["id"],"type":"function","function":{"name":data["name"],"arguments":data["arguments"]},"wire_ids":data["wire_ids"]});if let Some(sig)=data.get("thought_signature"){call["thought_signature"]=sig.clone();}
                        response["message"]["tool_calls"].as_array_mut().unwrap().push(call);response["finish_reason"]=json!("tool_calls");},
                    Some("usage")=>response["usage"]=data.clone(),
                    Some("done")=>{done=true;if let Some(finish)=data.get("finish_reason"){response["finish_reason"]=finish.clone();}
                        if let Some(served)=data.get("x-llmshim-served-model"){response["x-llmshim-served-model"]=served.clone();}break;},
                    Some("error")=>{yield Ok(Event::default().event("error").data(error_from_event(wire,&data).to_string()));return;},
                    _=>{},
                }
            }
            if !done {yield Ok(Event::default().event("error").data(error_body(wire,"stream ended before completion").to_string()));return;}
            let blocks=reasoning.blocks();if !blocks.is_empty(){response["message"]["reasoning"]=json!(blocks);}
            let response_receipts=receipts.clone();
            let response_scope=scope.clone();
            let native=match receipt_executor.run(ReceiptWorkKind::Egress, move || response_from_chat(&response,wire,&response_receipts,&response_scope)).await {
                Ok(value)=>value,
                Err(ReceiptWorkError::Failed(error))=>{yield Ok(Event::default().event("error").data(error_body(wire,&error).to_string()));return;},
                Err(ReceiptWorkError::Busy)=>{yield Ok(Event::default().event("error").data(error_body(wire,"native replay metadata is busy; retry the request").to_string()));return;},
            };
            let mut native=native;
            if wire==Wire::Chat {
                // Text was already streamed. Emit completed reasoning/calls once.
                native["choices"][0]["message"].as_object_mut().unwrap().remove("content");
            } else {
                if text_started {yield Ok(Event::default().event("content_block_stop").data(json!({"type":"content_block_stop","index":0}).to_string()));}
                native["content"].as_array_mut().unwrap().retain(|block|block["type"]!="text");
            }
            for (event,data) in stream_frames(&native,wire){
                if wire==Wire::Messages && event.as_deref()==Some("message_start"){continue;}
                let data=if wire==Wire::Messages && text_started {
                    match serde_json::from_str::<Value>(&data) {
                        Ok(mut value)=>{if let Some(index)=value["index"].as_u64(){value["index"]=json!(index+1);}value.to_string()},
                        Err(_)=>data,
                    }
                }else{data};
                let mut out = Event::default();
                if let Some(event) = event {
                    out = out.event(event);
                }
                yield Ok(out.data(data));
            }
        };
        let generated = Sse::new(events)
            .keep_alive(axum::response::sse::KeepAlive::default())
            .into_response();
        return Response::from_parts(parts, generated.into_body());
    }
    let bytes = match to_bytes(body, 32 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => return fail(wire, StatusCode::BAD_GATEWAY, "response exceeds size limit"),
    };
    let value: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => {
            return fail(
                wire,
                StatusCode::BAD_GATEWAY,
                "invalid response from handler",
            )
        }
    };
    let native = if parts.status.is_success() {
        let response_receipts = receipts.clone();
        let response_scope = scope.clone();
        match receipt_executor
            .run(ReceiptWorkKind::Egress, move || {
                response_from_chat(&value, wire, &response_receipts, &response_scope)
            })
            .await
        {
            Ok(value) => value,
            Err(error) => {
                return receipt_work_failure(wire, error, StatusCode::INTERNAL_SERVER_ERROR)
            }
        }
    } else {
        if let Some(error) = parts.extensions.get::<crate::error::NormalizedError>() {
            render_error(wire, error, fallback_error_type(wire, parts.status))
        } else {
            error_for_status(
                wire,
                parts.status,
                value["error"]["message"]
                    .as_str()
                    .unwrap_or("request failed"),
            )
        }
    };
    if let Some(served) = native["x-llmshim-served-model"].as_str() {
        if let Ok(header) = served.parse() {
            parts.headers.insert("x-llmshim-served-model", header);
        }
    }
    parts.headers.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/json"),
    );
    Response::from_parts(parts, Body::from(native.to_string()))
}

pub fn stream_frames(response: &Value, wire: Wire) -> Vec<(Option<String>, String)> {
    let mut frames = Vec::new();
    if wire == Wire::Chat {
        let mut chunk = response.clone();
        chunk["object"] = json!("chat.completion.chunk");
        let mut message = chunk["choices"][0]
            .as_object_mut()
            .unwrap()
            .remove("message")
            .unwrap();
        if let Some(calls) = message["tool_calls"].as_array_mut() {
            for (index, call) in calls.iter_mut().enumerate() {
                call["index"] = json!(index);
            }
        }
        chunk["choices"][0]["delta"] = message;
        frames.push((None, chunk.to_string()));
        frames.push((None, "[DONE]".into()));
        return frames;
    }
    let mut start = response.clone();
    start["content"] = json!([]);
    start["stop_reason"] = Value::Null;
    start["usage"]["output_tokens"] = json!(0);
    frames.push((
        Some("message_start".into()),
        json!({"type":"message_start","message":start}).to_string(),
    ));
    for (index, block) in response["content"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        let mut start = block.clone();
        let mut deltas = Vec::new();
        match block["type"].as_str() {
            Some("text") => {
                start["text"] = json!("");
                deltas.push(json!({"type":"text_delta","text":block["text"]}));
            }
            Some("thinking") => {
                start["thinking"] = json!("");
                start["signature"] = json!("");
                deltas.push(json!({"type":"thinking_delta","thinking":block["thinking"]}));
                deltas.push(json!({"type":"signature_delta","signature":block["signature"]}));
            }
            Some("tool_use") => {
                start["input"] = json!({});
                deltas.push(
                    json!({"type":"input_json_delta","partial_json":block["input"].to_string()}),
                );
            }
            _ => {}
        }
        frames.push((
            Some("content_block_start".into()),
            json!({"type":"content_block_start","index":index,"content_block":start}).to_string(),
        ));
        for delta in deltas {
            frames.push((
                Some("content_block_delta".into()),
                json!({"type":"content_block_delta","index":index,"delta":delta}).to_string(),
            ));
        }
        frames.push((
            Some("content_block_stop".into()),
            json!({"type":"content_block_stop","index":index}).to_string(),
        ));
    }
    let mut delta = json!({"type":"message_delta","delta":{"stop_reason":response["stop_reason"],"stop_sequence":null},"usage":response["usage"]});
    if let Some(served) = response.get("x-llmshim-served-model") {
        delta["x-llmshim-served-model"] = served.clone();
    }
    frames.push((Some("message_delta".into()), delta.to_string()));
    frames.push((
        Some("message_stop".into()),
        json!({"type":"message_stop"}).to_string(),
    ));
    frames
}

/// Anthropic clients send x-api-key; an explicit Authorization header wins.
pub(crate) fn normalize_auth(headers: &mut axum::http::HeaderMap) {
    if !headers.contains_key(header::AUTHORIZATION) {
        if let Some(key) = headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
            if let Ok(value) = format!("Bearer {key}").parse() {
                headers.insert(header::AUTHORIZATION, value);
            }
        }
    }
}

#[cfg(test)]
mod async_receipt_tests {
    use super::*;
    use axum::{
        body::{to_bytes, Body},
        http::Request,
        response::{sse::Event, IntoResponse, Sse},
        routing::post,
        Extension, Router,
    };
    use fs2::FileExt;
    use futures::stream;
    use std::{convert::Infallible, fs::OpenOptions, sync::atomic::AtomicUsize, time::Duration};
    use tower::ServiceExt;

    fn canonical_response() -> Value {
        json!({
            "id":"response-id",
            "model":"local/test",
            "message":{
                "role":"assistant",
                "content":"",
                "tool_calls":[{
                    "id":"call_ls_shared",
                    "type":"function",
                    "function":{"name":"read","arguments":"{}"}
                }]
            },
            "finish_reason":"tool_calls",
            "usage":{}
        })
    }

    fn native_app(default_store: DefaultReceiptStore) -> Router {
        Router::new()
            .route(
                "/v1/chat/completions",
                post(|| async { Json(canonical_response()) }),
            )
            .layer(axum::middleware::from_fn(translate))
            .layer(axum::middleware::from_fn(bound_inference_request_json))
            .layer(axum::middleware::from_fn_with_state(
                default_store,
                install_default_receipt_store,
            ))
    }

    fn request() -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"model":"local/test","messages":[{"role":"user","content":"hi"}]})
                    .to_string(),
            ))
            .unwrap()
    }

    #[tokio::test]
    async fn native_request_complexity_is_rejected_before_handler_with_wire_shape() {
        let receipts = Arc::new(Receipts::new(tempfile::tempdir().unwrap().keep()));
        let application = native_app(DefaultReceiptStore::new(receipts));
        let wide = (0..32_768).map(|_| 0).collect::<Vec<_>>();
        let response = application
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "model":"local/test",
                            "messages":[],
                            "ignored":wide
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
        assert_eq!(body["error"]["type"], "invalid_request_error");
        assert!(body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("complexity"));
    }

    fn gated_native_app(
        default_store: DefaultReceiptStore,
        reached_handler: Arc<tokio::sync::Notify>,
        release_handler: Arc<tokio::sync::Notify>,
        streaming: bool,
    ) -> Router {
        Router::new()
            .route(
                "/v1/chat/completions",
                post(move || {
                    let reached_handler = reached_handler.clone();
                    let release_handler = release_handler.clone();
                    async move {
                        reached_handler.notify_one();
                        release_handler.notified().await;
                        if streaming {
                            let events = vec![
                                Ok::<_, Infallible>(Event::default().data(
                                    json!({"type":"tool_call","id":"call_ls_overlap","name":"read","arguments":"{}"}).to_string(),
                                )),
                                Ok(Event::default().data(
                                    json!({"type":"done","finish_reason":"tool_calls"}).to_string(),
                                )),
                            ];
                            Sse::new(stream::iter(events)).into_response()
                        } else {
                            Json(canonical_response()).into_response()
                        }
                    }
                }),
            )
            .layer(axum::middleware::from_fn(translate))
            .layer(axum::middleware::from_fn_with_state(
                default_store,
                install_default_receipt_store,
            ))
    }

    #[tokio::test]
    async fn native_http_reuses_default_index_and_honors_explicit_receipts() {
        let default_directory = tempfile::tempdir().unwrap();
        let default_receipts = Arc::new(Receipts::new(default_directory.path().to_owned()));
        let application = native_app(DefaultReceiptStore::new(default_receipts.clone()));

        for _ in 0..2 {
            let response = application.clone().oneshot(request()).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let _ = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        }
        assert_eq!(default_receipts.full_reload_count(), 1);

        let sentinel_directory = tempfile::tempdir().unwrap();
        let sentinel_receipts = Arc::new(Receipts::new(sentinel_directory.path().to_owned()));
        let override_directory = tempfile::tempdir().unwrap();
        let override_receipts = Arc::new(Receipts::new(override_directory.path().to_owned()));
        let overridden = native_app(DefaultReceiptStore::new(sentinel_receipts.clone()))
            .layer(Extension(override_receipts.clone()));
        let response = overridden.oneshot(request()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(override_receipts.full_reload_count(), 1);
        assert_eq!(sentinel_receipts.full_reload_count(), 0);
        assert!(std::fs::read_dir(sentinel_directory.path())
            .unwrap()
            .next()
            .is_none());
    }

    #[tokio::test]
    async fn reserved_egress_completes_unary_and_streams_during_ingress_overlap() {
        for streaming in [false, true] {
            let receipt_directory = tempfile::tempdir().unwrap();
            let store = DefaultReceiptStore::new(Arc::new(Receipts::new(
                receipt_directory.path().to_owned(),
            )));
            let reached_handler = Arc::new(tokio::sync::Notify::new());
            let release_handler = Arc::new(tokio::sync::Notify::new());
            let application = gated_native_app(
                store.clone(),
                reached_handler.clone(),
                release_handler.clone(),
                streaming,
            );
            let mut native_request = request();
            if streaming {
                *native_request.body_mut() = Body::from(
                    json!({"model":"local/test","stream":true,"messages":[{"role":"user","content":"hi"}]}).to_string(),
                );
            }
            let response_task = tokio::spawn(application.oneshot(native_request));
            reached_handler.notified().await;

            let (started_sender, started_receiver) = tokio::sync::oneshot::channel();
            let (release_sender, release_receiver) = std::sync::mpsc::channel();
            let executor = store.executor.clone();
            let ingress_task = tokio::spawn(async move {
                executor
                    .run(ReceiptWorkKind::Ingress, move || {
                        let _ = started_sender.send(());
                        release_receiver.recv().unwrap();
                        Ok(())
                    })
                    .await
            });
            started_receiver.await.unwrap();
            release_handler.notify_one();

            let response = tokio::time::timeout(Duration::from_secs(2), response_task)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body = String::from_utf8(
                to_bytes(response.into_body(), 1024 * 1024)
                    .await
                    .unwrap()
                    .to_vec(),
            )
            .unwrap();
            assert!(body.contains("call_ls_"), "{body}");
            if streaming {
                assert!(body.contains("data: [DONE]"), "{body}");
                assert!(!body.contains("event: error"), "{body}");
            }
            release_sender.send(()).unwrap();
            assert!(ingress_task.await.unwrap().is_ok());
        }
    }

    #[tokio::test]
    async fn bounded_queue_cancellation_never_schedules_late_ingress_work() {
        let executor = ReceiptExecutor::new();
        let (active_started_sender, active_started_receiver) = tokio::sync::oneshot::channel();
        let (active_release_sender, active_release_receiver) = std::sync::mpsc::channel();
        let active_executor = executor.clone();
        let active = tokio::spawn(async move {
            active_executor
                .run(ReceiptWorkKind::Ingress, move || {
                    let _ = active_started_sender.send(());
                    active_release_receiver.recv().unwrap();
                    Ok(())
                })
                .await
        });
        active_started_receiver.await.unwrap();
        active.abort();

        let executed = Arc::new(AtomicUsize::new(0));
        let mut queued = Vec::new();
        for _ in 0..(RECEIPT_INGRESS_CAPACITY - 1) {
            let executor = executor.clone();
            let executed = executed.clone();
            queued.push(tokio::spawn(async move {
                executor
                    .run(ReceiptWorkKind::Ingress, move || {
                        executed.fetch_add(1, Ordering::Relaxed);
                        Ok(())
                    })
                    .await
            }));
        }
        tokio::task::yield_now().await;
        queued[0].abort();
        tokio::task::yield_now().await;

        let replacement_executor = executor.clone();
        let replacement_executed = executed.clone();
        queued.push(tokio::spawn(async move {
            replacement_executor
                .run(ReceiptWorkKind::Ingress, move || {
                    replacement_executed.fetch_add(1, Ordering::Relaxed);
                    Ok(())
                })
                .await
        }));
        tokio::time::timeout(Duration::from_secs(1), async {
            while executor.ingress_capacity.available_permits() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(matches!(
            executor.run(ReceiptWorkKind::Ingress, || Ok(())).await,
            Err(ReceiptWorkError::Busy)
        ));
        assert_eq!(executed.load(Ordering::Relaxed), 0);

        assert_eq!(
            executor
                .run(ReceiptWorkKind::Egress, || Ok(42_u8))
                .await
                .unwrap(),
            42
        );
        active_release_sender.send(()).unwrap();
        for task in queued.into_iter().skip(1) {
            assert!(task.await.unwrap().is_ok());
        }
        assert_eq!(
            executed.load(Ordering::Relaxed),
            RECEIPT_INGRESS_CAPACITY - 1
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn lock_contention_keeps_runtime_live_and_times_out_safely() {
        let receipt_directory = tempfile::tempdir().unwrap();
        let mut receipts = Receipts::new(receipt_directory.path().to_owned());
        receipts.set_lock_timeout(Duration::from_millis(100));
        let receipts = Arc::new(receipts);
        receipts.get("scope", "call", &json!("initialize")).unwrap();
        let lock_path = receipt_directory.path().join(".receipt-retention-v1.lock");
        let lock_file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(lock_path)
            .unwrap();
        lock_file.lock_exclusive().unwrap();

        let application = native_app(DefaultReceiptStore::new(receipts));
        let blocked_application = application.clone();
        let blocked = tokio::spawn(async move { blocked_application.oneshot(request()).await });
        tokio::time::sleep(Duration::from_millis(30)).await;
        tokio::time::timeout(Duration::from_millis(100), tokio::task::yield_now())
            .await
            .unwrap();
        let overloaded = tokio::time::timeout(Duration::from_millis(500), blocked)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(overloaded.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(overloaded.headers()[header::RETRY_AFTER], "1");

        lock_file.unlock().unwrap();
        let restored = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let response = application.clone().oneshot(request()).await.unwrap();
                if response.status() != StatusCode::SERVICE_UNAVAILABLE {
                    return response;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(restored.status(), StatusCode::OK);
    }
}
