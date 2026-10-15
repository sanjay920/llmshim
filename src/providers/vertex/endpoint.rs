//! Explicit project/location routing; every model is encoded as one path segment.
use super::{GoogleIdentity, VertexError};
use crate::error::Result;

pub(super) fn root(identity: &GoogleIdentity, publisher: &str) -> Result<String> {
    let valid = |s: &str| {
        !s.is_empty()
            && s.len() <= 128
            && s.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    };
    if !valid(&identity.project) || !valid(&identity.region) {
        return Err(VertexError::service(400, "InvalidProjectOrLocation").into_shim(None));
    }
    let host = match identity.region.as_str() {
        "global" => "aiplatform.googleapis.com".to_owned(),
        "us" | "eu" => format!("aiplatform.{}.rep.googleapis.com", identity.region),
        region => format!("{region}-aiplatform.googleapis.com"),
    };
    Ok(format!(
        "https://{host}/v1/projects/{}/locations/{}/publishers/{publisher}/models/",
        identity.project, identity.region
    ))
}
pub(super) fn model_segment(model: &str) -> Result<String> {
    if model.is_empty() || model.len() > 1024 {
        return Err(VertexError::service(400, "InvalidModel").into_shim(None));
    }
    Ok(model
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~@".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect())
}
