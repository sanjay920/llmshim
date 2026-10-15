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
    matches!(model, "claude-fable-5" | "claude-fable-5-1")
}
pub(super) fn requires_adaptive_thinking(model: &str) -> bool {
    is_fable(model) || matches!(model, "claude-opus-5-5" | "claude-sonnet-5-5")
}
/// Signed thinking binds these models to the conversation prefix.
pub(super) fn preserves_bound_system_turns(model: &str) -> bool {
    matches!(
        model,
        "claude-fable-5-1" | "claude-opus-5-5" | "claude-sonnet-5-5"
    )
}
fn rejects_assistant_prefill(model: &str) -> bool {
    is_fable(model) || model == "claude-opus-5-5"
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
    if requires_adaptive_thinking(model) || model == "claude-opus-5" {
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
    if rejects_assistant_prefill(model) && last_role == Some("assistant") {
        return Err(provider_error(
            400,
            concat!(
                "This Claude model does not support assistant prefill; ",
                "end the request with a user turn"
            ),
        ));
    }
    if preserves_bound_system_turns(model)
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
