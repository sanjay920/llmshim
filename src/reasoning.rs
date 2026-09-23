//! Typed, lossless reasoning and a single fail-closed replay policy.
//! Opaque data is never inspected to decide where a block may be sent.
mod normalize;
mod request_budget;
pub(crate) use normalize::capture_response_with_budget;
#[cfg(test)]
pub(crate) use normalize::capture_stream_with_budget;
pub use normalize::{capture_response, capture_stream, reasoning_text, ReasoningAccumulator};

use crate::catalog::{self, ModelFamily};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicU64, Ordering},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WireFormat {
    AnthropicMessages,
    #[serde(rename = "openai-responses")]
    OpenAiResponses,
    #[serde(rename = "openai-chat")]
    OpenAiChat,
    GoogleGenerateContent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningKind {
    Text,
    Redacted,
    Encrypted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningOrigin {
    pub provider: String,
    pub model: String,
    /// None explicitly records an unknown family; it never authorizes replay.
    pub family: Option<ModelFamily>,
    pub wire: WireFormat,
    pub received_at: DateTime<Utc>,
    /// Non-secret issuer binding. Missing bindings cannot authorize encryption replay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReasoningBlock {
    pub kind: ReasoningKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    pub origin: ReasoningOrigin,
    /// Original structured item, including ordered summary/content parts. This
    /// preserves fields that cannot be represented by a single text string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<Value>,
    /// Chat-compatible transports use several distinct reasoning containers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_field: Option<String>,
}

impl ReasoningBlock {
    pub fn text(text: impl Into<String>, origin: ReasoningOrigin) -> Self {
        Self {
            kind: ReasoningKind::Text,
            text: Some(text.into()),
            data: None,
            signature: None,
            item_id: None,
            origin,
            payload: None,
            source_field: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThoughtSignature {
    pub data: String,
    pub origin: ReasoningOrigin,
}

#[derive(Debug, Clone)]
pub struct ReplayTarget {
    pub provider: String,
    pub model: String,
    pub family: Option<ModelFamily>,
    pub wire: WireFormat,
    pub account: Option<String>,
}

impl ReplayTarget {
    pub fn new(provider: &str, model: &str, wire: WireFormat) -> Self {
        // `lookup_id` normalizes a known OpenRouter variant suffix (`:nitro`,
        // `:floor`, …) for this lookup only; `model` itself is stored below
        // unchanged, since it still has to go out on the wire as given.
        let family = catalog::lookup_id(&format!("{provider}/{model}"))
            .or_else(|| catalog::lookup_id(model))
            .and_then(|m| m.family);
        Self {
            provider: provider.into(),
            model: model.into(),
            family,
            wire,
            account: None,
        }
    }

    /// API-key bindings deliberately fail closed across key rotation. An OAuth
    /// adapter can instead supply its stable issuer account id as `identity`.
    /// Only a domain-separated hash is serialized, never the credential/id.
    pub fn bind_account(mut self, endpoint: &str, identity: Option<&str>) -> Self {
        self.account = identity.filter(|s| !s.is_empty()).map(|identity| {
            let mut h = Sha256::new();
            for field in [
                "llmshim-replay-account-v1",
                &self.provider,
                endpoint.trim_end_matches('/'),
                identity,
            ] {
                h.update((field.len() as u64).to_be_bytes());
                h.update(field.as_bytes());
            }
            format!("sha256:{:x}", h.finalize())
        });
        self
    }

    pub fn origin(&self) -> ReasoningOrigin {
        ReasoningOrigin {
            provider: self.provider.clone(),
            model: self.model.clone(),
            family: self.family,
            wire: self.wire,
            received_at: Utc::now(),
            account: self.account.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    MissingOrigin,
    UnknownFamily,
    FamilyMismatch,
    WireMismatch,
    AccountMismatch,
    Malformed,
    MissingSignature,
    ProviderMismatch,
}
impl DropReason {
    fn index(self) -> usize {
        self as usize
    }
    fn name(self) -> &'static str {
        match self {
            Self::MissingOrigin => "missing_origin",
            Self::UnknownFamily => "unknown_family",
            Self::FamilyMismatch => "family_mismatch",
            Self::WireMismatch => "wire_mismatch",
            Self::AccountMismatch => "account_mismatch",
            Self::Malformed => "malformed",
            Self::MissingSignature => "missing_signature",
            Self::ProviderMismatch => "provider_mismatch",
        }
    }
}
static DROPPED: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];

pub fn replay_counters() -> BTreeMap<&'static str, u64> {
    [
        DropReason::MissingOrigin,
        DropReason::UnknownFamily,
        DropReason::FamilyMismatch,
        DropReason::WireMismatch,
        DropReason::AccountMismatch,
        DropReason::Malformed,
        DropReason::MissingSignature,
        DropReason::ProviderMismatch,
    ]
    .into_iter()
    .map(|r| (r.name(), DROPPED[r.index()].load(Ordering::Relaxed)))
    .collect()
}
fn dropped(reason: DropReason) {
    DROPPED[reason.index()].fetch_add(1, Ordering::Relaxed);
}

pub fn should_replay(
    origin: &ReasoningOrigin,
    kind: ReasoningKind,
    target: &ReplayTarget,
) -> Result<(), DropReason> {
    if origin.provider.is_empty() || origin.model.is_empty() {
        return Err(DropReason::MissingOrigin);
    }
    match (origin.family, target.family) {
        (Some(a), Some(b)) if a == b => {}
        (None, _) | (_, None) => return Err(DropReason::UnknownFamily),
        _ => return Err(DropReason::FamilyMismatch),
    }
    if origin.wire != target.wire {
        return Err(DropReason::WireMismatch);
    }
    if origin.provider == "anthropic" && target.provider == "anthropic" {
        let source_model = origin.model.as_str();
        let target_model = target.model.as_str();
        let opus_55_source_supported = source_model != "claude-opus-5-5"
            || matches!(
                target_model,
                "claude-opus-5-5" | "claude-fable-5-1" | "claude-mythos-5-1"
            );
        let opus_55_target_supported = target_model != "claude-opus-5-5"
            || !(source_model.starts_with("claude-fable")
                || source_model.starts_with("claude-mythos"));
        if !opus_55_source_supported || !opus_55_target_supported {
            return Err(DropReason::FamilyMismatch);
        }
    }
    if kind == ReasoningKind::Encrypted
        && (origin.provider != target.provider
            || origin.account.is_none()
            || origin.account != target.account)
    {
        return Err(DropReason::AccountMismatch);
    }
    Ok(())
}

/// Compatibility reader for one release. Legacy fields without a recorded
/// origin are consumed and dropped; a destination must never be used as origin.
fn legacy_blocks(message: &Value) -> Vec<Value> {
    let Some(origin) = message.get("reasoning_origin") else {
        if [
            "reasoning_content",
            "reasoning_signature",
            "redacted_reasoning_content",
            "reasoning_details",
            "thinking_blocks",
        ]
        .iter()
        .any(|k| message.get(k).is_some())
            || message["reasoning"].is_string()
        {
            dropped(DropReason::MissingOrigin);
        }
        return Vec::new();
    };
    let Ok(origin) = serde_json::from_value::<ReasoningOrigin>(origin.clone()) else {
        dropped(DropReason::Malformed);
        return Vec::new();
    };
    normalize::legacy_chat_blocks(message, &origin)
        .into_iter()
        .map(|b| json!(b))
        .collect()
}

pub(crate) fn strip_fields(message: &mut Value) {
    if let Some(obj) = message.as_object_mut() {
        for key in [
            "reasoning",
            "reasoning_content",
            "reasoning_signature",
            "redacted_reasoning_content",
            "reasoning_origin",
            "reasoning_details",
            "thinking_blocks",
        ] {
            obj.remove(key);
        }
    }
}

fn filter_reasoning_after_preflight(message: &mut Value, target: &ReplayTarget) {
    let blocks = match message["reasoning"].as_array() {
        Some(a) => a.clone(),
        None => legacy_blocks(message),
    };
    strip_fields(message);
    let mut kept = Vec::new();
    for value in blocks {
        let result = if value.get("origin").is_none_or(Value::is_null) {
            Err(DropReason::MissingOrigin)
        } else {
            serde_json::from_value::<ReasoningBlock>(value.clone())
                .map_err(|_| DropReason::Malformed)
                .and_then(|b| {
                    should_replay(&b.origin, b.kind, target).and_then(|_| {
                        if b.kind == ReasoningKind::Text && b.data.is_some()
                            || b.kind != ReasoningKind::Encrypted
                                && b.payload.as_ref().is_some_and(|p| {
                                    p["encrypted_content"].is_string()
                                        || p["type"] == "reasoning.encrypted"
                                })
                        {
                            return Err(DropReason::Malformed);
                        }
                        if b.kind == ReasoningKind::Text && b.text.is_none() && b.payload.is_none()
                            || matches!(b.kind, ReasoningKind::Encrypted | ReasoningKind::Redacted)
                                && b.data.is_none()
                        {
                            return Err(DropReason::Malformed);
                        }
                        if target.wire == WireFormat::AnthropicMessages
                            && b.kind == ReasoningKind::Text
                            && b.signature.as_deref().is_none_or(str::is_empty)
                        {
                            return Err(DropReason::MissingSignature);
                        }
                        Ok(())
                    })
                })
        };
        match result {
            Ok(()) if message["role"] == "assistant" => kept.push(value),
            Ok(()) => dropped(DropReason::Malformed),
            Err(reason) => dropped(reason),
        }
    }
    if !kept.is_empty() {
        message["reasoning"] = json!(kept);
    }
    if let Some(calls) = message.get_mut("tool_calls").and_then(Value::as_array_mut) {
        for call in calls {
            if crate::toolcall::remove_nested_signature(call).is_some() {
                dropped(DropReason::MissingOrigin);
            }

            if let Some(sig) = call.get("thought_signature") {
                let result = if sig.is_string() {
                    Err(DropReason::MissingOrigin)
                } else {
                    serde_json::from_value::<ThoughtSignature>(sig.clone())
                        .map_err(|_| DropReason::Malformed)
                        .and_then(|s| {
                            if s.data.is_empty() {
                                Err(DropReason::MissingSignature)
                            } else if s.origin.provider != target.provider {
                                Err(DropReason::ProviderMismatch)
                            } else {
                                should_replay(&s.origin, ReasoningKind::Text, target)
                            }
                        })
                };
                if let Err(reason) = result {
                    call.as_object_mut().unwrap().remove("thought_signature");
                    dropped(reason);
                }
            }
        }
    }
}

/// Apply the shared replay policy to one message. Matching JSON blocks remain
/// unchanged; unknown or mismatched data is removed and counted. A derived
/// metadata limit refusal returns before `message` is mutated.
pub fn filter_reasoning_for_target(
    message: &mut Value,
    target: &ReplayTarget,
) -> crate::error::Result<()> {
    request_budget::preflight_message(message)?;
    filter_reasoning_after_preflight(message, target);
    Ok(())
}

pub(crate) fn blocks(message: &Value) -> impl Iterator<Item = ReasoningBlock> + '_ {
    message["reasoning"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| serde_json::from_value(v.clone()).ok())
}

pub(crate) fn anthropic_blocks(message: &Value) -> Vec<Value> {
    blocks(message)
        .filter_map(|b| {
            if let Some(payload) = b.payload.as_ref() {
                if b.kind == ReasoningKind::Text && payload["type"] == "thinking"
                    || b.kind == ReasoningKind::Redacted && payload["type"] == "redacted_thinking"
                {
                    return Some(payload.clone());
                }
            }
            match b.kind {
                ReasoningKind::Text => {
                    Some(json!({"type":"thinking","thinking":b.text?,"signature":b.signature?}))
                }
                ReasoningKind::Redacted => Some(json!({"type":"redacted_thinking","data":b.data?})),
                ReasoningKind::Encrypted => None,
            }
        })
        .collect()
}

pub(crate) fn responses_items(message: &Value) -> Vec<Value> {
    blocks(message)
        .filter(|b| b.payload.is_some() || b.item_id.is_some() || b.data.is_some())
        .map(|b| {
            if let Some(payload) = b.payload.filter(|p| p["type"] == "reasoning") {
                return payload;
            }
            let mut item = json!({"type":"reasoning","summary":[]});
            if let Some(id) = b.item_id {
                item["id"] = json!(id);
            }
            if let Some(data) = b.data {
                item["encrypted_content"] = json!(data);
            }
            if let Some(text) = b.text {
                item["summary"] = json!([{"type":"summary_text","text":text}]);
            }
            item
        })
        .collect()
}

pub(crate) fn gemini_parts(message: &Value) -> Vec<Value> {
    blocks(message)
        .filter_map(|b| {
            let mut part = json!({"thought":true,"text":b.text?});
            if let Some(sig) = b.signature {
                part["thoughtSignature"] = json!(sig);
            }
            Some(part)
        })
        .collect()
}

fn render_chat(message: &mut Value, target: &ReplayTarget) {
    let values: Vec<_> = blocks(message).collect();
    strip_fields(message);
    let mut text = String::new();
    let mut bare = String::new();
    let mut details = Vec::new();
    let mut thinking = Vec::new();
    for b in values {
        match (b.source_field.as_deref(), b.payload.as_ref()) {
            (Some("reasoning_details"), Some(p)) => {
                details.push(p.clone());
                if target.family == Some(ModelFamily::Deepseek) && target.provider != "openrouter" {
                    if let Some(t) = b.text.as_ref() {
                        text.push_str(t);
                    }
                }
            }
            (Some("thinking_blocks"), Some(p)) => thinking.push(p.clone()),
            (Some("reasoning"), _)
                if b.origin.provider == target.provider
                    && target.family != Some(ModelFamily::Deepseek) =>
            {
                if let Some(t) = b.text {
                    bare.push_str(&t);
                }
            }
            _ => {
                if let Some(t) = b.text {
                    text.push_str(&t);
                }
            }
        }
    }
    if !text.is_empty() {
        message["reasoning_content"] = json!(text);
    }
    if !bare.is_empty() {
        message["reasoning"] = json!(bare);
    }
    if !details.is_empty() {
        message["reasoning_details"] = json!(details);
    }
    if !thinking.is_empty() {
        message["thinking_blocks"] = json!(thinking);
    }
}

pub(crate) fn preflight_request(request: &Value) -> crate::error::Result<()> {
    request_budget::preflight_request(request)
}

pub(crate) fn prepare_request(
    request: &Value,
    target: &ReplayTarget,
) -> crate::error::Result<Value> {
    request_budget::preflight_request(request)?;
    let mut request = request.clone();
    if let Some(messages) = request.get_mut("messages").and_then(Value::as_array_mut) {
        for message in messages {
            filter_reasoning_after_preflight(message, target);
            // A raw native thinking block has no provenance and must not bypass
            // the policy simply by being put into an assistant content array.
            if let Some(parts) = message.get_mut("content").and_then(Value::as_array_mut) {
                parts.retain(|p| {
                    let untracked = matches!(
                        p["type"].as_str(),
                        Some("thinking" | "redacted_thinking" | "reasoning")
                    ) || p["thought"] == true;
                    if untracked {
                        dropped(DropReason::MissingOrigin);
                    }
                    !untracked
                });
            }
            if target.wire == WireFormat::OpenAiChat {
                render_chat(message, target);
            }
            if let Some(calls) = message.get_mut("tool_calls").and_then(Value::as_array_mut) {
                for call in calls {
                    if matches!(
                        target.wire,
                        WireFormat::GoogleGenerateContent | WireFormat::OpenAiChat
                    ) {
                        if let Some(data) = call.pointer("/thought_signature/data").cloned() {
                            call["thought_signature"] = data;
                        }
                    } else if let Some(obj) = call.as_object_mut() {
                        obj.remove("thought_signature");
                    }
                }
            }
        }
    }
    // Native escape hatches cannot smuggle untracked reasoning past the
    // canonical message policy. Replayable blocks belong in `messages`.
    sanitize_extensions(&mut request);
    Ok(request)
}

pub(crate) fn sanitize_extensions(request: &mut Value) {
    if let Some(obj) = request.as_object_mut() {
        for (key, extension) in obj.iter_mut().filter(|(key, _)| key.starts_with("x-")) {
            let _ = key;
            for field in ["input", "messages", "contents"] {
                if let Some(value) = extension.get_mut(field) {
                    strip_untracked_native(value);
                }
            }
        }
    }
}

fn strip_untracked_native(value: &mut Value) {
    let Some(items) = value.as_array_mut() else {
        return;
    };
    items.retain(|item| {
        let reasoning = matches!(
            item["type"].as_str(),
            Some("thinking" | "redacted_thinking" | "reasoning")
        ) || item["thought"] == true;
        if reasoning {
            dropped(DropReason::MissingOrigin);
        }
        !reasoning
    });
    for message in items {
        if [
            "reasoning",
            "reasoning_content",
            "reasoning_signature",
            "redacted_reasoning_content",
            "reasoning_details",
            "thinking_blocks",
        ]
        .iter()
        .any(|key| message.get(key).is_some())
        {
            dropped(DropReason::MissingOrigin);
        }
        strip_fields(message);
        for key in ["content", "parts"] {
            if let Some(parts) = message.get_mut(key) {
                strip_untracked_native(parts);
            }
        }
        if let Some(obj) = message.as_object_mut() {
            obj.remove("thoughtSignature");
            obj.remove("thought_signature");
        }
        if let Some(calls) = message.get_mut("tool_calls").and_then(Value::as_array_mut) {
            for call in calls {
                if let Some(obj) = call.as_object_mut() {
                    obj.remove("thought_signature");
                }
            }
        }
    }
}

pub(crate) fn enforce_stateless(body: &mut Value) -> crate::error::Result<()> {
    body["store"] = json!(false);
    body.as_object_mut().unwrap().remove("previous_response_id");
    body.as_object_mut().unwrap().remove("conversation");
    if body.get("include").is_none() {
        body["include"] = json!([]);
    }
    let include =
        body["include"]
            .as_array_mut()
            .ok_or_else(|| crate::error::ShimError::ProviderError {
                status: 400,
                body: "include must be an array".into(),
                retry_after: None,
            })?;
    if !include.iter().any(|v| v == "reasoning.encrypted_content") {
        include.push(json!("reasoning.encrypted_content"));
    }
    Ok(())
}

/// Rebind adapter-created metadata to the immutable HTTP request context. This
/// matters when OAuth credentials/account selection change while a stream runs.
pub(crate) fn bind_response_context_unchecked(response: &mut Value, target: &ReplayTarget) {
    let target_family = json!(target.family);
    let target_wire = json!(target.wire);
    let Some(choices) = response.get_mut("choices").and_then(Value::as_array_mut) else {
        return;
    };
    for choice in choices {
        for key in ["message", "delta"] {
            let Some(message) = choice.get_mut(key) else {
                continue;
            };
            if let Some(blocks) = message.get_mut("reasoning").and_then(Value::as_array_mut) {
                for block in blocks {
                    bind_origin(&mut block["origin"], target, &target_family, &target_wire);
                }
            }
            if let Some(calls) = message.get_mut("tool_calls").and_then(Value::as_array_mut) {
                for call in calls {
                    if let Some(origin) = call.pointer_mut("/thought_signature/origin") {
                        bind_origin(origin, target, &target_family, &target_wire);
                    }
                }
            }
        }
    }
}
fn bind_origin(
    origin: &mut Value,
    target: &ReplayTarget,
    target_family: &Value,
    target_wire: &Value,
) {
    replace_string_if_changed(origin, "provider", &target.provider);
    replace_string_if_changed(origin, "model", &target.model);
    if origin["family"] != *target_family {
        origin["family"] = target_family.clone();
    }
    if origin["wire"] != *target_wire {
        origin["wire"] = target_wire.clone();
    }
    if let Some(account) = &target.account {
        replace_string_if_changed(origin, "account", account);
    } else if let Some(obj) = origin.as_object_mut() {
        obj.remove("account");
    }
}

fn replace_string_if_changed(container: &mut Value, field: &str, replacement: &str) {
    if container[field].as_str() != Some(replacement) {
        container[field] = Value::String(replacement.to_owned());
    }
}
