//! Vertex declares its stricter signed-context boundary beside the transport definition.
use crate::reasoning::{DropReason, ReasoningOrigin, ReplayTarget};

pub(crate) fn restriction(origin: &ReasoningOrigin, target: &ReplayTarget) -> Option<DropReason> {
    let vertex = |name: &str| matches!(name, "vertex-gemini" | "vertex-anthropic");
    if !vertex(&origin.provider) && !vertex(&target.provider) {
        return None;
    }
    if origin.provider != target.provider {
        return Some(DropReason::ProviderMismatch);
    }
    if origin.account.is_none() || origin.account != target.account {
        return Some(DropReason::AccountMismatch);
    }
    if target.provider == "vertex-anthropic"
        && !crate::reasoning::bound_reader_allows(
            canonical_model(&origin.model),
            canonical_model(&target.model),
        )
    {
        return Some(DropReason::FamilyMismatch);
    }
    None
}

pub(crate) fn canonical_model(model: &str) -> &str {
    model.split('@').next().unwrap_or(model)
}
