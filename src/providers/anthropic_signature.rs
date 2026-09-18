//! Best-effort inspection of an undocumented signature envelope. This is an
//! observation, never authentication or an input to routing/replay decisions.
use serde::Serialize;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};

static MISMATCHES: AtomicU64 = AtomicU64::new(0);
pub fn mismatch_count() -> u64 {
    MISMATCHES.load(Ordering::Relaxed)
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Integrity {
    Ok,
    Mismatch {
        requested: String,
        served: Vec<String>,
    },
    Unknown,
}

/// Decode outer field 2 -> envelope field 1 -> header field 6. Unknown versions,
/// truncation, duplicates, invalid UTF-8, and unreasonable values are normal None.
#[cfg(feature = "signature-introspection")]
pub fn serving_model_from_signature(signature: &str) -> Option<String> {
    use base64::Engine;
    if signature.len() > 1024 * 1024 {
        return None;
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(signature)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(signature))
        .ok()?;
    let envelope = field(&bytes, 2)?;
    let header = field(envelope, 1)?;
    let model = std::str::from_utf8(field(header, 6)?).ok()?;
    if model.is_empty()
        || model.len() > 128
        || !model.as_bytes()[0].is_ascii_alphanumeric()
        || !model
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:/-".contains(&b))
    {
        return None;
    }
    Some(model.to_string())
}
#[cfg(not(feature = "signature-introspection"))]
pub fn serving_model_from_signature(_: &str) -> Option<String> {
    None
}

#[cfg(feature = "signature-introspection")]
fn varint(bytes: &[u8], at: &mut usize) -> Option<u64> {
    let mut result = 0;
    for shift in (0..70).step_by(7) {
        let b = *bytes.get(*at)?;
        *at += 1;
        if shift == 63 && b > 1 {
            return None;
        }
        result |= ((b & 127) as u64) << shift;
        if b & 128 == 0 {
            return Some(result);
        }
    }
    None
}
#[cfg(feature = "signature-introspection")]
fn field(bytes: &[u8], wanted: u64) -> Option<&[u8]> {
    let mut at = 0usize;
    let mut found = None;
    while at < bytes.len() {
        let tag = varint(bytes, &mut at)?;
        let number = tag >> 3;
        if number == 0 || number > 0x1fffffff {
            return None;
        }
        let wire = tag & 7;
        let length = match wire {
            0 => {
                varint(bytes, &mut at)?;
                0
            }
            1 => 8,
            2 => usize::try_from(varint(bytes, &mut at)?).ok()?,
            5 => 4,
            _ => return None,
        };
        let end = at.checked_add(length)?;
        let value = bytes.get(at..end)?;
        if number == wanted {
            if wire != 2 || found.is_some() {
                return None;
            }
            found = Some(value);
        }
        at = end;
    }
    found
}

fn comparison_id(model: &str) -> String {
    let name = model.strip_prefix("anthropic/").unwrap_or(model);
    let metadata = crate::catalog::resolve(&format!("anthropic/{name}"));
    let mut normalized = metadata
        .as_ref()
        .map(|m| m.name.as_str())
        .unwrap_or(name)
        .to_ascii_lowercase()
        .replace(['.', '_'], "-");
    if let Some((base, date)) = normalized.rsplit_once('-') {
        if date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit()) {
            normalized = base.into();
        }
    }
    normalized
}

/// Inspect only signed Claude blocks carried on the native Anthropic wire.
/// Does not mutate messages, increment metrics, or trust the signature contents.
pub fn inspect(response: &Value, requested: &str) -> Integrity {
    let mut served = std::collections::BTreeSet::new();
    for choice in response["choices"].as_array().into_iter().flatten() {
        for block in choice["message"]["reasoning"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if block["origin"]["wire"] != "anthropic-messages"
                || block["origin"]["family"] != "claude"
            {
                continue;
            }
            if let Some(model) = block["signature"]
                .as_str()
                .and_then(serving_model_from_signature)
            {
                served.insert(model);
            }
        }
    }
    if served.is_empty() {
        Integrity::Unknown
    } else if served
        .iter()
        .all(|served| comparison_id(served) == comparison_id(requested))
    {
        Integrity::Ok
    } else {
        Integrity::Mismatch {
            requested: requested.into(),
            served: served.into_iter().collect(),
        }
    }
}

/// Add response-level observability only; stored assistant messages stay exact.
pub(crate) fn observe(response: &mut Value, requested: &str) {
    if let Integrity::Mismatch { served, .. } = inspect(response, requested) {
        MISMATCHES.fetch_add(1, Ordering::Relaxed);
        response["x-llmshim-served-model"] = json!(served.join(","));
    }
}
