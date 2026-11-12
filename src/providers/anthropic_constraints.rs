//! Model constraints shared by Messages and Converse; sampling names use Messages spelling.
use crate::error::{Result, ShimError};
fn provider_error(status: u16, message: &str) -> ShimError {
    ShimError::ProviderError {
        status,
        body: message.into(),
        retry_after: None,
    }
}
use serde_json::{Map, Value};

pub(super) fn is_fable(model: &str) -> bool {
    matches!(
        model,
        "claude-fable-5" | "claude-fable-5-1" | "claude-fable-5-2"
    )
}
pub(super) fn requires_adaptive_thinking(model: &str) -> bool {
    is_fable(model) || matches!(model, "claude-opus-5-5" | "claude-sonnet-5-5")
}
/// Haiku 5.5 thinks adaptively but, unlike the models above, may disable
/// thinking and accepts forced tool choice. It rejects manual thinking,
/// sampling parameters and assistant prefill.
fn is_haiku_5_5(model: &str) -> bool {
    model == "claude-haiku-5-5"
}
/// Models that accept only `auto` or `none` tool choice.
fn rejects_forced_tools(model: &str) -> bool {
    matches!(
        model,
        "claude-fable-5-1" | "claude-fable-5-2" | "claude-opus-5-5" | "claude-sonnet-5-5"
    )
}
/// Signed thinking binds these models to the conversation prefix.
pub(super) fn preserves_bound_system_turns(model: &str) -> bool {
    rejects_forced_tools(model) || is_haiku_5_5(model)
}
fn rejects_assistant_prefill(model: &str) -> bool {
    is_fable(model) || matches!(model, "claude-opus-5-5" | "claude-haiku-5-5")
}
pub(super) fn clamp_effort<'a>(model: &str, effort: &'a str) -> &'a str {
    if effort == "none" && requires_adaptive_thinking(model) {
        "low"
    } else {
        effort
    }
}

/// Converse strips topP during active thinking; Messages keeps its existing top_p handling.
pub(super) fn apply(
    model: &str,
    body: &mut Map<String, Value>,
    converse: bool,
    last_role: Option<&str>,
) -> Result<()> {
    let thinking = body.get("thinking");
    let active = thinking
        .and_then(|t| t["type"].as_str())
        .is_some_and(|kind| matches!(kind, "enabled" | "adaptive"));
    if active {
        body.remove("temperature");
        body.remove("top_k");
        if converse {
            body.remove("top_p");
        }
    }
    if requires_adaptive_thinking(model) || matches!(model, "claude-opus-5" | "claude-haiku-5-5") {
        for key in ["temperature", "top_p", "top_k"] {
            body.remove(key);
        }
    }
    if requires_adaptive_thinking(model)
        && body
            .get("thinking")
            .is_some_and(|thinking| thinking["type"] != "adaptive")
    {
        return Err(provider_error(
            400,
            "This Claude model requires adaptive thinking; use reasoning_effort to control depth",
        ));
    }
    if is_haiku_5_5(model)
        && body
            .get("thinking")
            .is_some_and(|thinking| thinking["type"] == "enabled")
    {
        return Err(provider_error(
            400,
            "This Claude model rejects manual thinking budgets; use adaptive thinking with reasoning_effort",
        ));
    }
    if rejects_assistant_prefill(model) && last_role == Some("assistant") {
        return Err(provider_error(
            400,
            concat!(
                "This Claude model does not support assistant prefill; ",
                "end the request with a user turn"
            ),
        ));
    }
    if rejects_forced_tools(model)
        && body
            .get("tool_choice")
            .and_then(|choice| choice["type"].as_str())
            .is_some_and(|kind| matches!(kind, "any" | "tool"))
    {
        return Err(provider_error(
            400,
            concat!(
                "This Claude model supports only auto or none tool choice; ",
                "request the desired tool in the prompt"
            ),
        ));
    }
    Ok(())
}
