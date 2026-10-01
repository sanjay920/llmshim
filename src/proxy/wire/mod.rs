//! Native inbound API facades over the existing proxy/gateway handlers.
mod gemini;
mod receipts;
mod response;
mod responses;
pub use response::{response_from_chat, stream_frames};

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
pub(crate) use responses::stream_identity;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    convert::Infallible,
    io::{self, Write},
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
    restoration_limits: receipts::RestorationLimits,
    canonical_maximum_bytes: usize,
}

impl DefaultReceiptStore {
    pub(crate) fn from_env() -> Self {
        Self::new(Arc::new(Receipts::from_env()))
    }

    pub(crate) fn new(receipts: Arc<Receipts>) -> Self {
        Self {
            receipts,
            executor: ReceiptExecutor::new(),
            restoration_limits: receipts::RestorationLimits::default(),
            canonical_maximum_bytes: receipts::REQUEST_MAX_SERIALIZED_BYTES,
        }
    }

    #[cfg(test)]
    fn with_request_limits(
        receipts: Arc<Receipts>,
        restoration_limits: receipts::RestorationLimits,
        canonical_maximum_bytes: usize,
    ) -> Self {
        Self {
            receipts,
            executor: ReceiptExecutor::new(),
            restoration_limits,
            canonical_maximum_bytes,
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
    let path = request.uri().path();
    if request.method() != axum::http::Method::POST
        || (path != "/v1/chat" && path != "/v1/chat/stream" && native_route(path).is_none())
    {
        return next.run(request).await;
    }
    let path = path.to_owned();
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
    match native_route(path) {
        Some(route) => fail(route.wire, status, message),
        None => (
            status,
            Json(json!({
                "error": {
                    "code": if status == StatusCode::PAYLOAD_TOO_LARGE {
                        "request_too_large"
                    } else {
                        "invalid_request"
                    },
                    "message": message
                }
            })),
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
    Responses,
    Messages,
    Gemini,
}

/// A native inbound request's wire, plus what its path names that the body
/// cannot: Gemini puts both the model and the streaming action in the URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeRoute {
    pub wire: Wire,
    /// The model the path names. `None` when the body carries it.
    pub model: Option<String>,
    /// Whether the path's action streams. `false` when the body carries the
    /// `stream` flag instead.
    pub streams: bool,
}

/// The native inbound route a path serves, or `None` for every other path.
///
/// One function decides this so the proxy, the gateway and the request lifetime
/// cannot disagree about which paths are native — a disagreement that would
/// leave a wire outside admission control.
pub fn native_route(path: &str) -> Option<NativeRoute> {
    let (wire, model, streams) = match path {
        "/v1/chat/completions" => (Wire::Chat, None, false),
        "/v1/messages" => (Wire::Messages, None, false),
        "/v1/responses" => (Wire::Responses, None, false),
        path => {
            let (model, streams) = gemini::path_route(path)?;
            (Wire::Gemini, Some(model.to_owned()), streams)
        }
    };
    Some(NativeRoute {
        wire,
        model,
        streams,
    })
}
type Result<T> = std::result::Result<T, String>;

struct BoundedCanonicalWriter {
    bytes: Vec<u8>,
    maximum_bytes: usize,
    #[cfg(test)]
    growth_steps: usize,
}

impl Write for BoundedCanonicalWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let remaining = self
            .maximum_bytes
            .checked_sub(self.bytes.len())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::OutOfMemory,
                    receipts::REQUEST_LIMIT_ERROR_MESSAGE,
                )
            })?;
        if buffer.len() > remaining {
            return Err(io::Error::new(
                io::ErrorKind::OutOfMemory,
                receipts::REQUEST_LIMIT_ERROR_MESSAGE,
            ));
        }
        let required_capacity = self.bytes.len().checked_add(buffer.len()).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::OutOfMemory,
                receipts::REQUEST_LIMIT_ERROR_MESSAGE,
            )
        })?;
        if required_capacity > self.bytes.capacity() {
            let mut next_capacity = self.bytes.capacity().max(64).min(self.maximum_bytes);
            while next_capacity < required_capacity {
                next_capacity = next_capacity
                    .checked_mul(2)
                    .unwrap_or(self.maximum_bytes)
                    .min(self.maximum_bytes);
                if next_capacity < required_capacity && next_capacity == self.maximum_bytes {
                    return Err(io::Error::new(
                        io::ErrorKind::OutOfMemory,
                        receipts::REQUEST_LIMIT_ERROR_MESSAGE,
                    ));
                }
            }
            let additional_capacity =
                next_capacity.checked_sub(self.bytes.len()).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::OutOfMemory,
                        receipts::REQUEST_LIMIT_ERROR_MESSAGE,
                    )
                })?;
            self.bytes
                .try_reserve_exact(additional_capacity)
                .map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::OutOfMemory,
                        receipts::REQUEST_LIMIT_ERROR_MESSAGE,
                    )
                })?;
            #[cfg(test)]
            {
                self.growth_steps += 1;
            }
        }
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn serialize_canonical_request(value: &Value, maximum_bytes: usize) -> Result<Vec<u8>> {
    let mut writer = BoundedCanonicalWriter {
        bytes: Vec::new(),
        maximum_bytes,
        #[cfg(test)]
        growth_steps: 0,
    };
    serde_json::to_writer(&mut writer, value)
        .map_err(|_| receipts::REQUEST_LIMIT_ERROR_MESSAGE.to_owned())?;
    Ok(writer.bytes)
}
fn array<'a>(value: &'a Value, name: &str) -> Result<&'a Vec<Value>> {
    value
        .as_array()
        .ok_or_else(|| format!("{name} must be an array"))
}
fn native_call(call: &Value) -> Value {
    json!({
        "id": call["id"],
        "type": "function",
        "function": {
            "name": call["function"]["name"],
            "arguments": call["function"]["arguments"]
        }
    })
}
fn call_content(call: &Value) -> Result<Value> {
    let args = call["function"]["arguments"]
        .as_str()
        .ok_or("tool arguments must be JSON text")?;
    let value: Value =
        match crate::json_bounds::parse_str(args, crate::json_bounds::Limits::INBOUND) {
            Ok(value) => value,
            Err(crate::json_bounds::ParseError::Malformed(_)) => {
                return Err("invalid tool argument JSON".into())
            }
            Err(crate::json_bounds::ParseError::Complexity) => {
                return Err("tool arguments exceed JSON complexity limit".into())
            }
        };
    Ok(json!({"id": call["id"],"name": call["function"]["name"],"arguments": value}))
}
fn import_call(
    call: &Value,
    receipts: &Receipts,
    scope: &str,
    restoration_budget: &mut receipts::RestorationBudget,
) -> Result<Value> {
    if let Some(issued) = receipts.get_bounded(scope, "call", &call["id"], restoration_budget)? {
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
    json!({"error": {
        "message": message,
        "type": "invalid_request_error",
        "param": param,
        "code": "unsupported_parameter",
    }})
    .to_string()
}

fn message_key(message: &Value) -> Value {
    json!({
        "content": message["content"],
        "reasoning_content": message["reasoning_content"],
        "reasoning": message["reasoning"],
        "reasoning_details": message["reasoning_details"],
        "thinking_blocks": message["thinking_blocks"]
    })
}

pub fn request_to_chat(
    native: &Value,
    wire: Wire,
    receipts: &Receipts,
    scope: &str,
) -> Result<Value> {
    request_to_chat_with_limits(
        native,
        wire,
        receipts,
        scope,
        receipts::RestorationLimits::default(),
    )
}

fn request_to_chat_with_limits(
    native: &Value,
    wire: Wire,
    receipts: &Receipts,
    scope: &str,
    restoration_limits: receipts::RestorationLimits,
) -> Result<Value> {
    if wire == Wire::Responses {
        let mut chat = request_to_chat_with_limits(
            &responses::request(native)?,
            Wire::Chat,
            receipts,
            scope,
            restoration_limits,
        )?;
        responses::restore(&mut chat, receipts, scope, restoration_limits)?;
        return enforce_canonical_bounds(chat, restoration_limits);
    }
    let mut restoration_budget = receipts::RestorationBudget::new(restoration_limits);
    if wire == Wire::Gemini {
        let canonical = gemini::request(native, receipts, scope, &mut restoration_budget)?;
        return enforce_canonical_bounds(canonical, restoration_limits);
    }
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
                Wire::Chat | Wire::Responses => tool["type"] == "function",
                Wire::Messages => tool.get("type").is_none() || tool["type"] == "custom",
                // Unreachable: the Gemini reader above validated its own
                // declarations, whose entries carry no `type`.
                Wire::Gemini => tool.get("type").is_none(),
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
            messages.push(json!({"role": "system","content": system}));
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
                // A message exported before `reasoning_details` existed carries
                // the same array under `reasoning`. Alias it so the receipt key
                // matches, then read the receipt through the canonical field.
                let mut key_source = message.clone();
                if key_source.get("reasoning_details").is_none()
                    && key_source.get("reasoning").is_some_and(Value::is_array)
                {
                    key_source["reasoning_details"] = key_source["reasoning"].clone();
                    key_source.as_object_mut().unwrap().remove("reasoning");
                }
                if let Some(reasoning) = receipts.get_bounded(
                    scope,
                    "reasoning",
                    &message_key(&key_source),
                    &mut restoration_budget,
                )? {
                    canonical["reasoning"] = reasoning;
                }
                if let Some(calls) = message.get("tool_calls") {
                    canonical["tool_calls"] = Value::Array(
                        array(calls, "tool_calls")?
                            .iter()
                            .map(|call| import_call(call, receipts, scope, &mut restoration_budget))
                            .collect::<Result<Vec<_>>>()?,
                    );
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
                    let call = json!({
                        "id": block["id"],
                        "type": "function",
                        "function": {
                            "name": block["name"],
                            "arguments": block["input"].to_string()
                        }
                    });
                    let mut call = import_call(&call, receipts, scope, &mut restoration_budget)?;
                    if let Some(cache) = block.get("cache_control") {
                        call["cache_control"] = cache.clone();
                    }
                    calls.push(call);
                }
                Some("tool_result") if role == "user" => {
                    let mut result = json!({
                        "role": "tool",
                        "tool_call_id": block["tool_use_id"],
                        "content": block["content"]
                    });
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
                    if let Some(original) =
                        receipts.get_bounded(scope, "block", block, &mut restoration_budget)?
                    {
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
            let mut canonical = json!({"role": role,"content": content});
            if !calls.is_empty() {
                canonical["tool_calls"] = Value::Array(calls);
            }
            if !reasoning.is_empty() {
                canonical["reasoning"] = Value::Array(reasoning);
            }
            messages.push(canonical);
        }
        boundaries.push(messages.len().checked_sub(1));
    }
    let mut config = obj
        .iter()
        .filter(|(key, _)| {
            !matches!(
                key.as_str(),
                "model" | "messages" | "system" | "stream" | "n"
            )
        })
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<serde_json::Map<String, Value>>();
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
                    json!({"type": "json_schema","json_schema": {"schema": format["schema"]}}),
                );
            }
        }
        if let Some(tools) = native.get("tools") {
            config.insert(
                "tools".into(),
                json!(array(tools, "tools")?
                    .iter()
                    .map(|tool| {
                        let mut out = json!({
                            "type": "function",
                            "function": {
                                "name": tool["name"],
                                "parameters": tool["input_schema"]
                            }
                        });
                        for field in ["description", "strict"] {
                            if let Some(value) = tool.get(field) {
                                out["function"][field] = value.clone();
                            }
                        }
                        if let Some(cache) = tool.get("cache_control") {
                            out["cache_control"] = cache.clone();
                        }
                        out
                    })
                    .collect::<Vec<_>>()),
            );
        }
        if let Some(choice) = native.get("tool_choice") {
            let choice = match choice["type"].as_str() {
                Some("any") => json!("required"),
                Some("auto") => json!("auto"),
                Some("none") => json!("none"),
                Some("tool") => json!({"type": "function","function": {"name": choice["name"]}}),
                _ => return Err("invalid tool_choice".into()),
            };
            config.insert("tool_choice".into(), choice);
            if native["tool_choice"]["disable_parallel_tool_use"] == true {
                config.insert("parallel_tool_calls".into(), json!(false));
            }
        }
    }
    let mut canonical = json!({
        "model": model,
        "stream": native["stream"].as_bool().unwrap_or(false),
        "fallback": native["fallback"],
    });
    canonical["messages"] = Value::Array(messages);
    canonical["provider_config"] = Value::Object(config);
    enforce_canonical_bounds(canonical, restoration_limits)
}

/// The restored canonical request may not exceed the restoration budgets it was
/// read under, whichever wire produced it.
fn enforce_canonical_bounds(
    canonical: Value,
    limits: receipts::RestorationLimits,
) -> Result<Value> {
    crate::json_bounds::measure_value(
        &canonical,
        crate::json_bounds::Limits {
            max_depth: crate::json_bounds::Limits::INBOUND.max_depth,
            max_nodes: limits.max_nodes,
            max_owned_bytes: limits.max_owned_bytes,
        },
    )
    .map_err(|_| receipts::REQUEST_LIMIT_ERROR_MESSAGE.to_owned())?;
    Ok(canonical)
}

fn error_body(wire: Wire, message: &str) -> Value {
    render_error(
        wire,
        &crate::error::normalize_error(message),
        StatusCode::BAD_REQUEST,
    )
}

fn render_error(wire: Wire, error: &crate::error::NormalizedError, status: StatusCode) -> Value {
    let fallback_type = fallback_error_type(wire, status);
    match wire {
        Wire::Gemini => gemini::error(status, &error.message),
        Wire::Messages => {
            let kind = error
                .code_type()
                .or(error.kind.as_deref())
                .unwrap_or(fallback_type);
            let kind = if kind == "server_error" {
                "api_error"
            } else {
                kind
            };
            json!({"type": "error","error": {"type": kind,"message": error.message}})
        }
        Wire::Chat | Wire::Responses => {
            let kind = error
                .kind
                .as_deref()
                .or(error.code_type())
                .unwrap_or(fallback_type);
            json!({
                "error": {
                    "type": kind,
                    "message": error.message,
                    "param": error.param,
                    "code": error.code
                }
            })
        }
    }
}

/// A failure frame. Gemini's SSE carries no event names, so its failures are
/// bare `data:` frames holding Google's error object - the same shape the
/// request lifetime already emits for a timed-out stream.
fn error_frame(wire: Wire, body: Value) -> Event {
    let event = if wire == Wire::Gemini {
        Event::default()
    } else {
        Event::default().event("error")
    };
    event.data(body.to_string())
}

fn error_from_event(wire: Wire, event: &Value) -> Value {
    match event.get("error").filter(|error| error.is_object()) {
        Some(error) => error_body(wire, &json!({"error": error}).to_string()),
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
pub(crate) fn error_for_status(wire: Wire, status: StatusCode, message: &str) -> Value {
    render_error(wire, &crate::error::normalize_error(message), status)
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
    translate_with_router(request, next, None).await
}

pub(crate) async fn translate_with_router(
    request: Request,
    next: Next,
    router: Option<&crate::router::Router>,
) -> Response {
    if request.method() != axum::http::Method::POST {
        return next.run(request).await;
    }
    let Some(route) = native_route(request.uri().path()) else {
        // A `/v1beta/models/` path is a Gemini client asking for a method this
        // server does not serve (`:countTokens`, a misspelled action). Answering
        // in Google's shape keeps it out of the chat handler, where the same
        // body is either an axum deserialization rejection or - worse - a
        // completion the caller never asked for.
        if request.uri().path().starts_with("/v1beta/models/") {
            return fail(
                Wire::Gemini,
                StatusCode::NOT_FOUND,
                "this endpoint serves the generateContent and streamGenerateContent actions only",
            );
        }
        return next.run(request).await;
    };
    let wire = route.wire;
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
    let restoration_limits = default_store.restoration_limits;
    let canonical_maximum_bytes = default_store.canonical_maximum_bytes;
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
    let mut native: Value =
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
    if let Some(model) = &route.model {
        // Gemini names the model and the action in the URL; the body carries
        // neither, and nothing there may disagree with the path.
        native["model"] = json!(model);
        native["stream"] = json!(route.streams);
    }
    let response_model = native["model"].clone();
    let include_reasoning = native["include"].as_array().is_some_and(|items| {
        items
            .iter()
            .any(|item| item == "reasoning.encrypted_content")
    });
    let request_receipts = receipts.clone();
    let request_scope = scope.clone();
    let mut chat = match receipt_executor
        .run(ReceiptWorkKind::Ingress, move || {
            request_to_chat_with_limits(
                &native,
                wire,
                &request_receipts,
                &request_scope,
                restoration_limits,
            )
        })
        .await
    {
        Ok(value) => value,
        Err(ReceiptWorkError::Failed(message))
            if message == receipts::REQUEST_LIMIT_ERROR_MESSAGE =>
        {
            return fail(
                wire,
                StatusCode::PAYLOAD_TOO_LARGE,
                receipts::REQUEST_LIMIT_ERROR_MESSAGE,
            )
        }
        Err(error) => return receipt_work_failure(wire, error, StatusCode::BAD_REQUEST),
    };
    let replay_metadata = if wire == Wire::Responses {
        let destination = router.and_then(|router| {
            let req =
                serde_json::from_value::<crate::proxy::types::ChatRequest>(chat.clone()).ok()?;
            let prepared = crate::proxy::convert::prepare_request(router, &req).ok()?;
            let (provider, model) = router
                .resolve_owned(prepared.payload["model"].as_str()?)
                .ok()?;
            Some((provider, model, prepared.payload))
        });
        let result = receipt_executor
            .run(ReceiptWorkKind::Ingress, move || {
                let target = destination.and_then(|(provider, model, payload)| {
                    let native = provider.transform_request(&model, &payload).ok()?;
                    Some(provider.request_replay_target(&model, &native))
                });
                let metadata = responses::replay_metadata(&mut chat, target.as_ref())?;
                Ok((
                    enforce_canonical_bounds(chat, restoration_limits)?,
                    metadata,
                ))
            })
            .await;
        match result {
            Ok((prepared, metadata)) => {
                chat = prepared;
                metadata
            }
            Err(error) => return receipt_work_failure(wire, error, StatusCode::BAD_REQUEST),
        }
    } else {
        json!({})
    };
    let serialized_chat = match serialize_canonical_request(&chat, canonical_maximum_bytes) {
        Ok(serialized) => serialized,
        Err(message) => return fail(wire, StatusCode::PAYLOAD_TOO_LARGE, &message),
    };
    parts.headers.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/json"),
    );
    parts.headers.remove(header::CONTENT_LENGTH);
    let response = next
        .run(Request::from_parts(parts, Body::from(serialized_chat)))
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
        if wire == Wire::Responses {
            let generated = responses::stream_response(
                body,
                response_model,
                receipts,
                scope,
                receipt_executor,
                replay_metadata,
                include_reasoning,
            )
            .into_response();
            return Response::from_parts(parts, generated.into_body());
        }
        let model = response_model;
        let events = async_stream::stream! {
            let mut stream = Box::pin(crate::sse::data(body.into_data_stream()));
            let mut response = json!({
                "id": format!("msg_{}",
                uuid::Uuid::new_v4().simple()),
                "model": model,
                "message": {
                    "role": "assistant",
                    "content": ""
                },
                "usage": {
                },
                "finish_reason": "stop"
            });
            response["created"] = json!(chrono::Utc::now().timestamp());
            let mut text_started = false;
            let start = if wire == Wire::Chat {
                json!({
                    "id": response["id"],
                    "model": model,
                    "object": "chat.completion.chunk",
                    "created": response["created"],
                    "choices": [
                        {
                            "index": 0,
                            "delta": {
                                "role": "assistant"
                            },
                            "finish_reason": null
                        }
                    ]
                })
            } else {
                json!({
                    "type": "message_start",
                    "message": {
                        "id": response["id"],
                        "type": "message",
                        "role": "assistant",
                        "model": model,
                        "content": [
                        ],
                        "stop_reason": null,
                        "stop_sequence": null,
                        "usage": {
                            "input_tokens": 0,
                            "output_tokens": 0
                        }
                    }
                })
            };
            // Gemini's own chunks each carry the role, so that wire opens with
            // its first rendered frame rather than a synthetic role chunk whose
            // model would only be the one the caller asked for.
            if wire != Wire::Gemini {
                let mut start_event = Event::default();
                if wire == Wire::Messages {
                    start_event = start_event.event("message_start");
                }
                yield Ok::<Event, Infallible>(start_event.data(start.to_string()));
            }
            let mut reasoning = crate::reasoning::ReasoningAccumulator::default();
            let mut size = 0usize;
            let mut done = false;
            while let Some(event) = stream.next().await {
                let event = match event {
                    Ok(event) => event,
                    Err(_) => {
                        yield Ok::<Event, Infallible>(error_frame(
                            wire,
                            error_body(wire, "upstream stream failed"),
                        ));
                        return;
                    }
                };
                size = size.saturating_add(event.len());
                if size > 32 * 1024 * 1024 {
                    yield Ok(error_frame(
                        wire,
                        error_body(wire, "response exceeds size limit"),
                    ));
                    return;
                }
                let data: Value = match serde_json::from_str(&event) {
                    Ok(data) => data,
                    Err(_) => continue,
                };
                match data["type"].as_str() {
                    Some("content") => {
                        crate::streaming::append_string_fragment(
                            &mut response["message"]["content"],
                            data["text"].as_str().unwrap_or(""),
                        );
                        if wire == Wire::Chat {
                            yield Ok(Event::default().data(
                                json!({
                                    "id": response["id"],
                                    "model": model,
                                    "object": "chat.completion.chunk",
                                    "created": response["created"],
                                    "choices": [
                                        {
                                            "index": 0,
                                            "delta": {
                                                "content": data["text"]
                                            },
                                            "finish_reason": null
                                        }
                                    ]
                                })
                                .to_string(),
                            ));
                        } else if wire == Wire::Gemini {
                            yield Ok(Event::default().data(
                                json!({
                                    "candidates": [
                                        {
                                            "content": {
                                                "role": "model",
                                                "parts": [
                                                    {
                                                        "text": data["text"]
                                                    }
                                                ]
                                            },
                                            "index": 0
                                        }
                                    ]
                                })
                                .to_string(),
                            ));
                        } else {
                            if !text_started {
                                yield Ok(Event::default().event("content_block_start").data(
                                    json!({
                                        "type": "content_block_start",
                                        "index": 0,
                                        "content_block": {
                                            "type": "text",
                                            "text": ""
                                        }
                                    })
                                    .to_string(),
                                ));
                                text_started = true;
                            }
                            yield Ok(Event::default().event("content_block_delta").data(
                                json!({
                                    "type": "content_block_delta",
                                    "index": 0,
                                    "delta": {
                                        "type": "text_delta",
                                        "text": data["text"]
                                    }
                                })
                                .to_string(),
                            ));
                        }
                    }
                    Some("reasoning") => {
                        if reasoning
                            .push(&json!({
                                "reasoning": data["blocks"]
                            }))
                            .is_err()
                        {
                            yield Ok(error_frame(
                                wire,
                                error_body(wire, crate::stream_retention::RETENTION_ERROR),
                            ));
                            return;
                        }
                    }
                    Some("tool_call") => {
                        if !response["message"]["tool_calls"].is_array() {
                            response["message"]["tool_calls"] = json!([]);
                        }
                        let mut call = json!({
                            "id": data["id"],
                            "type": "function",
                            "function": {
                                "name": data["name"],
                                "arguments": data["arguments"]
                            },
                            "wire_ids": data["wire_ids"]
                        });
                        if let Some(sig) = data.get("thought_signature") {
                            call["thought_signature"] = sig.clone();
                        }
                        response["message"]["tool_calls"]
                            .as_array_mut()
                            .unwrap()
                            .push(call);
                        response["finish_reason"] = json!("tool_calls");
                    }
                    Some("usage") => response["usage"] = data.clone(),
                    Some("done") => {
                        done = true;
                        if let Some(finish) = data.get("finish_reason") {
                            response["finish_reason"] = finish.clone();
                        }
                        if let Some(served) = data.get("x-llmshim-served-model") {
                            response["x-llmshim-served-model"] = served.clone();
                        }
                        break;
                    }
                    Some("error") => {
                        yield Ok(error_frame(wire, error_from_event(wire, &data)));
                        return;
                    }
                    _ => {}
                }
            }
            if !done {
                yield Ok(error_frame(
                    wire,
                    error_body(wire, "stream ended before completion"),
                ));
                return;
            }
            let blocks = reasoning.blocks();
            if !blocks.is_empty() {
                response["message"]["reasoning"] = json!(blocks);
            }
            let response_receipts = receipts.clone();
            let response_scope = scope.clone();
            let native = match receipt_executor
                .run(ReceiptWorkKind::Egress, move || {
                    response_from_chat(&response, wire, &response_receipts, &response_scope)
                })
                .await
            {
                Ok(value) => value,
                Err(ReceiptWorkError::Failed(error)) => {
                    yield Ok(error_frame(wire, error_body(wire, &error)));
                    return;
                }
                Err(ReceiptWorkError::Busy) => {
                    yield Ok(error_frame(
                        wire,
                        error_body(wire, "native replay metadata is busy; retry the request"),
                    ));
                    return;
                }
            };
            let mut native = native;
            if wire == Wire::Chat {
                // Text was already streamed. Emit completed reasoning/calls once.
                native["choices"][0]["message"]
                    .as_object_mut()
                    .unwrap()
                    .remove("content");
            } else if wire == Wire::Gemini {
                // Only the answer text was streamed already. A thought part
                // carries `text` too, and reasoning has no other container on
                // this wire, so dropping it here would lose the block and its
                // signature from every streamed answer.
                native["candidates"][0]["content"]["parts"]
                    .as_array_mut()
                    .unwrap()
                    .retain(|part| part.get("thought").is_some() || part.get("text").is_none());
            } else {
                if text_started {
                    yield Ok(Event::default().event("content_block_stop").data(
                        json!({
                            "type": "content_block_stop",
                            "index": 0
                        })
                        .to_string(),
                    ));
                }
                native["content"]
                    .as_array_mut()
                    .unwrap()
                    .retain(|block| block["type"] != "text");
            }
            for (event, data) in stream_frames(&native, wire) {
                if wire == Wire::Messages && event.as_deref() == Some("message_start") {
                    continue;
                }
                let data = if wire == Wire::Messages && text_started {
                    match serde_json::from_str::<Value>(&data) {
                        Ok(mut value) => {
                            if let Some(index) = value["index"].as_u64() {
                                value["index"] = json!(index + 1);
                            }
                            value.to_string()
                        }
                        Err(_) => data,
                    }
                } else {
                    data
                };
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
    let mut native = if parts.status.is_success() {
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
            render_error(wire, error, parts.status)
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
    if wire == Wire::Responses && parts.status.is_success() {
        let mut response = match serde_json::from_value::<responses::Response>(native) {
            Ok(response) => response,
            Err(_) => {
                return fail(
                    wire,
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "malformed Responses output",
                );
            }
        };
        responses::output_options(&mut response, replay_metadata, include_reasoning);
        native = json!(response);
    }
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

/// Anthropic clients send x-api-key and Gemini clients send x-goog-api-key; an
/// explicit Authorization header wins over either.
pub(crate) fn normalize_auth(headers: &mut axum::http::HeaderMap) {
    if !headers.contains_key(header::AUTHORIZATION) {
        let key = headers
            .get("x-api-key")
            .or_else(|| headers.get("x-goog-api-key"))
            .and_then(|v| v.to_str().ok());
        if let Some(key) = key {
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

    fn anonymous_scope() -> String {
        format!("{:x}", Sha256::digest(b"anonymous"))
    }

    fn issued_call(id: &str) -> Value {
        json!({
            "id": id,
            "type": "function",
            "function": {"name": "read","arguments": "{}"},
            "thought_signature": {
                "data": "ordinary-signature",
                "origin": {
                    "provider": "gemini",
                    "model": "model",
                    "family": null,
                    "wire": "google-generate-content",
                    "received_at": "2026-09-22T00:00:00Z"
                }
            },
            "wire_ids": [
                {
                    "provider": "gemini",
                    "wire": "google-generate-content",
                    "scope": "scope",
                    "part_id": "0",
                    "id": null
                }
            ]
        })
    }

    fn canonical_response() -> Value {
        json!({
            "id": "response-id",
            "model": "local/test",
            "message": {
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "id": "call_ls_shared",
                    "type": "function",
                    "function": {"name": "read","arguments": "{}"}
                }]
            },
            "finish_reason": "tool_calls",
            "usage": {}
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
                json!({"model": "local/test","messages": [{"role": "user","content": "hi"}]})
                    .to_string(),
            ))
            .unwrap()
    }

    #[tokio::test]
    async fn repeated_valid_receipt_refuses_before_handler_and_releases_for_next_request() {
        let directory = tempfile::tempdir().unwrap();
        let receipts = Arc::new(Receipts::new(directory.path().to_owned()));
        let call = issued_call("call_ls_budget");
        receipts
            .put(&anonymous_scope(), "call", &call["id"], &call)
            .unwrap();
        let handler_calls = Arc::new(AtomicUsize::new(0));
        let counted = handler_calls.clone();
        let application = Router::new()
            .route(
                "/v1/chat/completions",
                post(move || {
                    let counted = counted.clone();
                    async move {
                        counted.fetch_add(1, Ordering::SeqCst);
                        Json(canonical_response())
                    }
                }),
            )
            .layer(axum::middleware::from_fn(translate))
            .layer(axum::middleware::from_fn(bound_inference_request_json))
            .layer(axum::middleware::from_fn_with_state(
                DefaultReceiptStore::with_request_limits(
                    receipts.clone(),
                    receipts::RestorationLimits {
                        max_serialized_bytes: 64 * 1024,
                        max_owned_bytes: 64 * 1024,
                        max_nodes: 256,
                        max_occurrences: 1,
                    },
                    2 * 1024 * 1024,
                ),
                install_default_receipt_store,
            ));
        let native_call = native_call(&call);
        let repeated = json!({
            "model": "local/test",
            "messages": [
                {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [
                        native_call.clone(),
                        native_call.clone()
                    ]
                }
            ]
        });
        let refused = application
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(Body::from(repeated.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(refused.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(handler_calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            receipts
                .get(&anonymous_scope(), "call", &call["id"])
                .unwrap(),
            Some(call.clone())
        );

        let valid = json!({
            "model": "local/test",
            "messages": [{"role": "assistant","content": null,"tool_calls": [native_call]}]
        });
        let accepted = application
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(Body::from(valid.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(accepted.status(), StatusCode::OK);
        assert_eq!(handler_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn canonical_serialization_limit_refuses_before_handler() {
        let receipt_directory = tempfile::tempdir().unwrap();
        let receipts = Arc::new(Receipts::new(receipt_directory.path().to_owned()));
        let handler_calls = Arc::new(AtomicUsize::new(0));
        let counted = handler_calls.clone();
        let application = Router::new()
            .route(
                "/v1/messages",
                post(move || {
                    let counted = counted.clone();
                    async move {
                        counted.fetch_add(1, Ordering::SeqCst);
                        Json(canonical_response())
                    }
                }),
            )
            .layer(axum::middleware::from_fn(translate))
            .layer(axum::middleware::from_fn(bound_inference_request_json))
            .layer(axum::middleware::from_fn_with_state(
                DefaultReceiptStore::with_request_limits(
                    receipts,
                    receipts::RestorationLimits::default(),
                    64,
                ),
                install_default_receipt_store,
            ));
        let response = application
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/messages")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "model": "local/test",
                            "messages": [
                                {
                                    "role": "user",
                                    "content": "ordinary"
                                }
                            ]
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
        assert_eq!(handler_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn canonical_writer_grows_geometrically_and_enforces_exact_limit() {
        let expected = (0_u8..100).collect::<Vec<_>>();
        let mut writer = BoundedCanonicalWriter {
            bytes: Vec::new(),
            maximum_bytes: 128,
            growth_steps: 0,
        };
        for byte in &expected {
            writer.write_all(&[*byte]).unwrap();
        }
        assert_eq!(writer.bytes, expected);
        assert!(writer.growth_steps <= 3);
        assert!(writer.write_all(&[7_u8; 28]).is_ok());
        let before_refusal = writer.bytes.clone();
        assert!(writer.write_all(&[8_u8]).is_err());
        assert_eq!(writer.bytes, before_refusal);
    }

    #[test]
    fn tool_and_reasoning_restores_share_one_occurrence_budget() {
        let directory = tempfile::tempdir().unwrap();
        let receipts = Receipts::new(directory.path().to_owned());
        let scope = "mixed-scope";
        let call = issued_call("call_ls_mixed");
        receipts.put(scope, "call", &call["id"], &call).unwrap();
        let exported_block = json!({"type": "redacted_thinking","data": "lsr_mixed"});
        let canonical_block = json!({
            "kind": "redacted",
            "data": "ordinary-redacted-data",
            "origin": {
                "provider": "openai",
                "model": "model",
                "family": null,
                "wire": "openai-responses",
                "received_at": "2026-09-22T00:00:00Z"
            },
            "payload": {"type": "reasoning","encrypted_content": "ordinary-redacted-data"}
        });
        receipts
            .put(scope, "block", &exported_block, &canonical_block)
            .unwrap();
        let native = json!({
            "model": "local/test",
            "messages": [{"role": "assistant","content": [
                {"type": "tool_use","id": "call_ls_mixed","name": "read","input": {}},
                exported_block
            ]}]
        });
        let error = request_to_chat_with_limits(
            &native,
            Wire::Messages,
            &receipts,
            scope,
            receipts::RestorationLimits {
                max_serialized_bytes: 64 * 1024,
                max_owned_bytes: 64 * 1024,
                max_nodes: 256,
                max_occurrences: 1,
            },
        )
        .unwrap_err();
        assert_eq!(error, receipts::REQUEST_LIMIT_ERROR_MESSAGE);

        let restored = request_to_chat(&native, Wire::Messages, &receipts, scope).unwrap();
        assert_eq!(restored["messages"][0]["reasoning"][0], canonical_block);
        assert_eq!(restored["messages"][0]["tool_calls"][0], call);
    }

    #[test]
    fn canonical_node_limit_is_checked_before_serialization() {
        let receipt_directory = tempfile::tempdir().unwrap();
        let receipts = Receipts::new(receipt_directory.path().to_owned());
        let native = json!({
            "model": "local/test",
            "messages": [{"role": "user","content": "ordinary"}]
        });
        let error = request_to_chat_with_limits(
            &native,
            Wire::Chat,
            &receipts,
            "scope",
            receipts::RestorationLimits {
                max_serialized_bytes: 1024,
                max_owned_bytes: 1024,
                max_nodes: 4,
                max_occurrences: 4,
            },
        )
        .unwrap_err();
        assert_eq!(error, receipts::REQUEST_LIMIT_ERROR_MESSAGE);
        assert_eq!(native["messages"][0]["content"], "ordinary");
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
                            "model": "local/test",
                            "messages": [],
                            "ignored": wide
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
                                Ok::<_, Infallible>(
                                    Event::default().data(
                                        json!({
                                            "type": "tool_call",
                                            "id": "call_ls_overlap",
                                            "name": "read",
                                            "arguments": "{}"
                                        })
                                        .to_string(),
                                    ),
                                ),
                                Ok(Event::default().data(
                                    json!({"type": "done","finish_reason": "tool_calls"})
                                        .to_string(),
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
                    json!({
                        "model": "local/test",
                        "stream": true,
                        "messages": [
                            {
                                "role": "user",
                                "content": "hi"
                            }
                        ]
                    })
                    .to_string(),
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
