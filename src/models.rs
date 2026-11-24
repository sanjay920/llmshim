//! Curated, verified discovery. Dynamic metadata lives in [`crate::catalog`].
//! Unverified facts stay `Support::Unknown` / `None`; never guess a number.

pub use llmshim_catalog::builtin::{
    available_models, spec, BuiltinModelInfo as ModelInfo, CHATGPT_MODELS, MODELS,
};
pub use llmshim_catalog::{ModelCapabilities, ModelFamily, Support};
