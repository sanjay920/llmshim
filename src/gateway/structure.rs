//! Prompt structure for the cache debugger (design note 0052, slice 5a).
//!
//! A provider's prompt cache keys on an exact prefix: everything from the first token up to a cache boundary must be
//! identical to an earlier request. This module cuts the **provider-native** request body (what the provider
//! actually received, after llmshim's translation) into **segments** in the order the provider reads them, and gives
//! each a keyed fingerprint. Two requests' segment lists can then be compared to find where the shared prefix ends,
//! without storing a word of either prompt.
//!
//! A segment is `{kind, hash, bytes}`. `hash` is the first 8 bytes of an HMAC-SHA-256, keyed by a deployment secret
//! the control plane never has, over the segment's kind and canonical JSON (object keys sorted, `cache_control`
//! markers removed, so moving a breakpoint does not look like a content change). Without the key a hash of a
//! guessable segment cannot be tested offline. Images and audio are a segment with a size and a hash, never bytes.
//!
//! | Wire | Order of segments |
//! |---|---|
//! | Anthropic Messages | `tools[]`, `system`, then each `messages[]` content block |
//! | OpenAI Responses | `tools[]`, `instructions`, then each `input[]` item (a message by its content parts) |
//! | Chat Completions | `tools[]`, then each `messages[]` entry (an array content by its parts) |
//! | Gemini | `tools[]`, `systemInstruction.parts[]`, then each `contents[].parts[]` |

// The module is the seam `serve` plugs its capture into; without that feature nothing opts a client in.
#![cfg_attr(not(feature = "serve"), allow(dead_code))]

use crate::reasoning::WireFormat;
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// The most segments one request reports; the tail beyond it is one `other` segment.
pub const MAX_SEGMENTS: usize = 2048;

/// One part of a prompt, by kind and keyed fingerprint.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Segment {
    /// `tool`, `system`, `user`, `assistant`, `tool_result`, `image` or `other`.
    pub kind: &'static str,
    /// 16 lower-case hex digits: the first 8 bytes of the keyed hash.
    pub hash: String,
    /// The size of the segment's canonical JSON, in bytes.
    pub bytes: u64,
}

/// The kinds a segment can have (the control plane accepts exactly these).
#[cfg(test)]
pub const KINDS: [&str; 7] = [
    "tool",
    "system",
    "user",
    "assistant",
    "tool_result",
    "image",
    "other",
];

/// HMAC-SHA-256 (RFC 2104), with the block size of SHA-256.
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut block = [0u8; BLOCK];
    if key.len() > BLOCK {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(block.map(|b| b ^ 0x36));
    inner.update(message);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(block.map(|b| b ^ 0x5c));
    outer.update(inner);
    outer.finalize().into()
}

/// The canonical form of a segment: object keys sorted at every level, `cache_control` removed, no whitespace.
fn canonical(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Object(map) => {
            out.push(b'{');
            let mut keys: Vec<&String> = map
                .keys()
                .filter(|k| k.as_str() != "cache_control")
                .collect();
            keys.sort();
            for (n, key) in keys.into_iter().enumerate() {
                if n > 0 {
                    out.push(b',');
                }
                out.extend_from_slice(Value::String(key.clone()).to_string().as_bytes());
                out.push(b':');
                canonical(&map[key], out);
            }
            out.push(b'}');
        }
        Value::Array(items) => {
            out.push(b'[');
            for (n, item) in items.iter().enumerate() {
                if n > 0 {
                    out.push(b',');
                }
                canonical(item, out);
            }
            out.push(b']');
        }
        other => out.extend_from_slice(other.to_string().as_bytes()),
    }
}

fn role_kind(role: &str) -> &'static str {
    match role {
        "user" => "user",
        "assistant" | "model" => "assistant",
        "system" | "developer" => "system",
        "tool" | "function" => "tool_result",
        _ => "other",
    }
}

/// Collects `(kind, canonical bytes)` in order, then fingerprints them.
struct Collected {
    parts: Vec<(&'static str, Vec<u8>)>,
}

impl Collected {
    fn push(&mut self, kind: &'static str, value: &Value) {
        let mut bytes = Vec::new();
        canonical(value, &mut bytes);
        self.parts.push((kind, bytes));
    }

    fn items(&mut self, kind: &'static str, value: Option<&Value>) {
        if let Some(Value::Array(items)) = value {
            for item in items {
                self.push(kind, item);
            }
        }
    }

    fn finish(self, key: &[u8]) -> Vec<Segment> {
        let segment = |kind: &'static str, bytes: &[u8]| {
            let mut message = Vec::with_capacity(bytes.len() + 16);
            message.extend_from_slice(kind.as_bytes());
            message.push(0);
            message.extend_from_slice(bytes);
            let mac = hmac_sha256(key, &message);
            Segment {
                kind,
                hash: mac[..8].iter().map(|b| format!("{b:02x}")).collect(),
                bytes: bytes.len() as u64,
            }
        };
        let mut parts = self.parts;
        let mut tail = Vec::new();
        if parts.len() > MAX_SEGMENTS {
            // The last segment stands for everything beyond the cap, so a change anywhere in the tail still shows.
            for (kind, bytes) in parts.drain(MAX_SEGMENTS - 1..) {
                tail.extend_from_slice(kind.as_bytes());
                tail.push(0);
                tail.extend_from_slice(&bytes);
                tail.push(0);
            }
        }
        let mut out: Vec<Segment> = parts
            .iter()
            .map(|(kind, bytes)| segment(kind, bytes))
            .collect();
        if !tail.is_empty() {
            out.push(segment("other", &tail));
        }
        out
    }
}

fn block_kind(role: &str, block: &Value) -> &'static str {
    match block.get("type").and_then(Value::as_str) {
        Some("text" | "input_text" | "output_text" | "refusal") | None => role_kind(role),
        Some("tool_use" | "thinking" | "redacted_thinking" | "server_tool_use") => "assistant",
        Some("tool_result") => "tool_result",
        Some("image" | "document" | "image_url" | "input_image" | "input_audio" | "input_file") => {
            "image"
        }
        Some(_) => "other",
    }
}

fn anthropic(body: &Value, c: &mut Collected) {
    c.items("tool", body.get("tools"));
    match body.get("system") {
        Some(Value::String(text)) => c.push("system", &json!(text)),
        Some(Value::Array(blocks)) => blocks.iter().for_each(|b| c.push("system", b)),
        _ => {}
    }
    for message in body
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let role = message.get("role").and_then(Value::as_str).unwrap_or("");
        match message.get("content") {
            Some(Value::String(text)) => {
                c.push(role_kind(role), &json!({"role": role, "content": text}))
            }
            Some(Value::Array(blocks)) => {
                for block in blocks {
                    c.push(
                        block_kind(role, block),
                        &json!({"role": role, "block": block}),
                    );
                }
            }
            _ => c.push(role_kind(role), message),
        }
    }
}

/// A chat-style message: an array content by its parts, then any other fields (tool calls, a tool call id) as one more.
fn chat_message(message: &Value, c: &mut Collected) {
    let role = message.get("role").and_then(Value::as_str).unwrap_or("");
    match message.get("content") {
        Some(Value::Array(parts)) => {
            for part in parts {
                c.push(block_kind(role, part), &json!({"role": role, "part": part}));
            }
            if let Some(object) = message.as_object() {
                let extras: serde_json::Map<String, Value> = object
                    .iter()
                    .filter(|(k, _)| k.as_str() != "content")
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                if extras.len() > 1 {
                    c.push(role_kind(role), &Value::Object(extras));
                }
            }
        }
        _ => c.push(role_kind(role), message),
    }
}

fn openai_responses(body: &Value, c: &mut Collected) {
    c.items("tool", body.get("tools"));
    if let Some(Value::String(text)) = body.get("instructions") {
        c.push("system", &json!(text));
    }
    match body.get("input") {
        Some(Value::String(text)) => c.push("user", &json!({"role": "user", "content": text})),
        Some(Value::Array(items)) => {
            for item in items {
                match item.get("type").and_then(Value::as_str) {
                    Some(
                        "function_call" | "reasoning" | "custom_tool_call" | "web_search_call",
                    ) => c.push("assistant", item),
                    Some("function_call_output" | "custom_tool_call_output") => {
                        c.push("tool_result", item)
                    }
                    _ if item.get("role").is_some() => chat_message(item, c),
                    _ => c.push("other", item),
                }
            }
        }
        _ => {}
    }
}

fn chat_completions(body: &Value, c: &mut Collected) {
    c.items("tool", body.get("tools"));
    for message in body
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        chat_message(message, c);
    }
}

fn gemini(body: &Value, c: &mut Collected) {
    c.items("tool", body.get("tools"));
    c.items(
        "system",
        body.get("systemInstruction").and_then(|s| s.get("parts")),
    );
    for content in body
        .get("contents")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let role = content
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        for part in content
            .get("parts")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let kind = if part.get("functionResponse").is_some() {
                "tool_result"
            } else if part.get("inlineData").is_some() || part.get("fileData").is_some() {
                "image"
            } else if part.get("functionCall").is_some() {
                "assistant"
            } else {
                role_kind(role)
            };
            c.push(kind, &json!({"role": role, "part": part}));
        }
    }
}

/// The segments of a provider-native request body, or `None` when the body is not one this wire's walker
/// understands (not an object, or none of the arrays the wire puts a prompt in). `key` is the deployment's capture
/// secret.
pub fn segments(wire: WireFormat, body: &Value, key: &[u8]) -> Option<Vec<Segment>> {
    let object = body.as_object()?;
    let present = match wire {
        WireFormat::AnthropicMessages | WireFormat::OpenAiChat => object.contains_key("messages"),
        WireFormat::OpenAiResponses => object.contains_key("input"),
        WireFormat::GoogleGenerateContent => object.contains_key("contents"),
    };
    if !present {
        return None;
    }
    let mut collected = Collected { parts: Vec::new() };
    match wire {
        WireFormat::AnthropicMessages => anthropic(body, &mut collected),
        WireFormat::OpenAiResponses => openai_responses(body, &mut collected),
        WireFormat::OpenAiChat => chat_completions(body, &mut collected),
        WireFormat::GoogleGenerateContent => gemini(body, &mut collected),
    }
    Some(collected.finish(key))
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &[u8] = b"a-deployment-secret-of-some-length";

    fn kinds(s: &[Segment]) -> Vec<&'static str> {
        s.iter().map(|x| x.kind).collect()
    }

    #[test]
    fn hmac_matches_the_published_vectors() {
        // RFC 4231, test case 1 and test case 6 (a key longer than the block).
        let mac = hmac_sha256(&[0x0b; 20], b"Hi There");
        assert_eq!(
            mac.iter().map(|b| format!("{b:02x}")).collect::<String>(),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        let mac = hmac_sha256(
            &[0xaa; 131],
            b"Test Using Larger Than Block-Size Key - Hash Key First",
        );
        assert_eq!(
            mac.iter().map(|b| format!("{b:02x}")).collect::<String>(),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn anthropic_orders_tools_then_system_then_messages_and_ignores_cache_markers_and_key_order() {
        let body = json!({
            "model": "m", "max_tokens": 10,
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "hi", "cache_control": {"type": "ephemeral"}}]},
                {"role": "assistant", "content": [{"type": "tool_use", "id": "t1", "name": "f", "input": {"b": 1, "a": 2}}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "ok"}, {"type": "image", "source": {"data": "AAAA"}}]},
            ],
            "system": [{"type": "text", "text": "be brief", "cache_control": {"type": "ephemeral"}}],
            "tools": [{"name": "f", "input_schema": {"type": "object"}}],
        });
        let s = segments(WireFormat::AnthropicMessages, &body, KEY).unwrap();
        assert_eq!(
            kinds(&s),
            [
                "tool",
                "system",
                "user",
                "assistant",
                "tool_result",
                "image"
            ]
        );
        // A moved or removed breakpoint and a different key order are the same prompt.
        let same = json!({
            "tools": [{"input_schema": {"type": "object"}, "name": "f"}],
            "system": [{"text": "be brief", "type": "text"}],
            "messages": [
                {"content": [{"text": "hi", "type": "text"}], "role": "user"},
                {"role": "assistant", "content": [{"type": "tool_use", "id": "t1", "name": "f", "input": {"a": 2, "b": 1}}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "ok"}, {"type": "image", "source": {"data": "AAAA"}}]},
            ],
        });
        assert_eq!(
            segments(WireFormat::AnthropicMessages, &same, KEY).unwrap(),
            s
        );
    }

    #[test]
    fn a_change_moves_that_segment_and_appending_moves_none_before_it() {
        let base = json!({"model": "m", "messages": [
            {"role": "system", "content": "You are helpful. 10:00"},
            {"role": "user", "content": "hello"},
        ], "tools": [{"type": "function", "function": {"name": "f"}}]});
        let s = segments(WireFormat::OpenAiChat, &base, KEY).unwrap();
        assert_eq!(kinds(&s), ["tool", "system", "user"]);
        let mut changed = base.clone();
        changed["messages"][0]["content"] = json!("You are helpful. 10:01");
        let c = segments(WireFormat::OpenAiChat, &changed, KEY).unwrap();
        assert_eq!(c[0], s[0]);
        assert_ne!(c[1], s[1]);
        assert_eq!(
            c[2], s[2],
            "only the changed segment differs, not the later ones"
        );
        let mut appended = base.clone();
        appended["messages"]
            .as_array_mut()
            .unwrap()
            .push(json!({"role": "assistant", "content": "hi!"}));
        let a = segments(WireFormat::OpenAiChat, &appended, KEY).unwrap();
        assert_eq!(&a[..3], &s[..]);
        assert_eq!(a.len(), 4);
        // A different key makes every hash different: nothing is comparable across keys.
        let other = segments(
            WireFormat::OpenAiChat,
            &base,
            b"another-secret-of-similar-length!!",
        )
        .unwrap();
        assert!(other
            .iter()
            .zip(&s)
            .all(|(x, y)| x.hash != y.hash && x.bytes == y.bytes));
    }

    #[test]
    fn the_kind_is_part_of_the_fingerprint() {
        let a = segments(
            WireFormat::OpenAiChat,
            &json!({"messages": [{"role": "user", "content": "x"}]}),
            KEY,
        )
        .unwrap();
        let b = segments(
            WireFormat::OpenAiChat,
            &json!({"messages": [{"role": "assistant", "content": "x"}]}),
            KEY,
        )
        .unwrap();
        assert_ne!(a[0].hash, b[0].hash);
    }

    #[test]
    fn chat_array_content_is_cut_by_its_parts_and_tool_calls_are_their_own_segment() {
        let body = json!({"messages": [
            {"role": "user", "content": [{"type": "text", "text": "look"}, {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}]},
            {"role": "assistant", "content": null, "tool_calls": [{"id": "c1", "type": "function", "function": {"name": "f", "arguments": "{}"}}]},
            {"role": "tool", "tool_call_id": "c1", "content": "42"},
        ]});
        let s = segments(WireFormat::OpenAiChat, &body, KEY).unwrap();
        assert_eq!(kinds(&s), ["user", "image", "assistant", "tool_result"]);
        // An image is a size and a hash: its bytes are not in any field.
        assert!(!serde_json::to_string(&s).unwrap().contains("AAAA"));
    }

    #[test]
    fn responses_and_gemini_follow_their_own_order() {
        let responses = json!({"instructions": "sys", "tools": [{"type": "function", "name": "f"}], "input": [
            {"role": "user", "content": [{"type": "input_text", "text": "q"}]},
            {"type": "function_call", "call_id": "1", "name": "f", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "1", "output": "ok"},
        ]});
        assert_eq!(
            kinds(&segments(WireFormat::OpenAiResponses, &responses, KEY).unwrap()),
            ["tool", "system", "user", "assistant", "tool_result"]
        );
        let gemini = json!({"systemInstruction": {"parts": [{"text": "sys"}]}, "tools": [{"functionDeclarations": []}], "contents": [
            {"role": "user", "parts": [{"text": "q"}, {"inlineData": {"mimeType": "image/png", "data": "AAAA"}}]},
            {"role": "model", "parts": [{"functionCall": {"name": "f", "args": {}}}]},
            {"role": "user", "parts": [{"functionResponse": {"name": "f", "response": {}}}]},
        ]});
        assert_eq!(
            kinds(&segments(WireFormat::GoogleGenerateContent, &gemini, KEY).unwrap()),
            [
                "tool",
                "system",
                "user",
                "image",
                "assistant",
                "tool_result"
            ]
        );
    }

    #[test]
    fn a_body_the_walker_does_not_understand_has_no_structure() {
        assert!(segments(WireFormat::AnthropicMessages, &json!("text"), KEY).is_none());
        assert!(segments(WireFormat::OpenAiChat, &json!({"prompt": "x"}), KEY).is_none());
        assert!(segments(
            WireFormat::GoogleGenerateContent,
            &json!({"messages": []}),
            KEY
        )
        .is_none());
        // An empty prompt is still a prompt: no segments, but understood.
        assert_eq!(
            segments(WireFormat::OpenAiChat, &json!({"messages": []}), KEY).unwrap(),
            []
        );
    }

    #[test]
    fn the_cap_folds_the_tail_into_one_segment_that_still_shows_a_change() {
        let messages = |last: &str| {
            let mut m: Vec<Value> = (0..MAX_SEGMENTS + 40)
                .map(|n| json!({"role": "user", "content": format!("m{n}")}))
                .collect();
            m.last_mut().unwrap()["content"] = json!(last);
            json!({"messages": m})
        };
        let a = segments(WireFormat::OpenAiChat, &messages("x"), KEY).unwrap();
        let b = segments(WireFormat::OpenAiChat, &messages("y"), KEY).unwrap();
        assert_eq!(a.len(), MAX_SEGMENTS);
        assert_eq!(a[MAX_SEGMENTS - 1].kind, "other");
        assert_eq!(a[..MAX_SEGMENTS - 1], b[..MAX_SEGMENTS - 1]);
        assert_ne!(a[MAX_SEGMENTS - 1].hash, b[MAX_SEGMENTS - 1].hash);
    }

    #[test]
    fn no_segment_carries_any_of_the_text() {
        let body = json!({"messages": [{"role": "user", "content": "the secret plan"}], "tools": [{"function": {"name": "launch"}}]});
        let text =
            serde_json::to_string(&segments(WireFormat::OpenAiChat, &body, KEY).unwrap()).unwrap();
        assert!(
            !text.contains("secret") && !text.contains("launch"),
            "{text}"
        );
        let s = segments(WireFormat::OpenAiChat, &body, KEY).unwrap();
        assert!(s
            .iter()
            .all(|x| x.hash.len() == 16 && x.hash.bytes().all(|b| b.is_ascii_hexdigit())));
        assert!(s.iter().all(|x| KINDS.contains(&x.kind)));
    }
}
