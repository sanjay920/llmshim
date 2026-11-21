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
use serde_json::Value;
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

/// HMAC-SHA-256 (RFC 2104), with the block size of SHA-256, fed in pieces so a large segment is hashed as it is
/// walked and never copied.
struct Mac {
    inner: Sha256,
    opad: [u8; 64],
}

impl Mac {
    fn new(key: &[u8]) -> Self {
        let mut block = [0u8; 64];
        if key.len() > 64 {
            block[..32].copy_from_slice(&Sha256::digest(key));
        } else {
            block[..key.len()].copy_from_slice(key);
        }
        let mut inner = Sha256::new();
        inner.update(block.map(|b| b ^ 0x36));
        Self {
            inner,
            opad: block.map(|b| b ^ 0x5c),
        }
    }

    fn update(&mut self, bytes: &[u8]) {
        self.inner.update(bytes);
    }

    fn finish(self) -> [u8; 32] {
        let inner = self.inner.finalize();
        let mut outer = Sha256::new();
        outer.update(self.opad);
        outer.update(inner);
        outer.finalize().into()
    }
}

/// HMAC-SHA-256 of one message (the vectors in the tests check [`Mac`] through it).
#[cfg(test)]
fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut mac = Mac::new(key);
    mac.update(message);
    mac.finish()
}

/// The most canonical bytes one request is hashed over. Capture runs on the request's path, so its work is
/// bounded: past this the request has no structure (counted as `too_large`) instead of costing it time.
pub const MAX_BYTES: u64 = 8 << 20;

/// Where canonical bytes go: into a MAC, counted, and refused once the budget is spent (which also stops the walk
/// of a value early).
struct Writer<'a> {
    mac: &'a mut Mac,
    len: u64,
    spent: &'a mut u64,
}

impl std::io::Write for Writer<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        *self.spent += bytes.len() as u64;
        if *self.spent > MAX_BYTES {
            return Err(std::io::Error::other("budget"));
        }
        self.mac.update(bytes);
        self.len += bytes.len() as u64;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

type Done = std::io::Result<()>;

fn put(out: &mut Writer<'_>, bytes: &[u8]) -> Done {
    std::io::Write::write_all(out, bytes)
}

/// A JSON string or other leaf, escaped as it is written (no copy of a large string).
fn leaf(value: &Value, out: &mut Writer<'_>) -> Done {
    serde_json::to_writer(out, value).map_err(std::io::Error::from)
}

/// The canonical form of a value: object keys sorted at every level, no whitespace. `strip_marker` removes a
/// `cache_control` key of **this** object only, where a provider's cache marker sits (a tool, a system block, a
/// content block); the same name deeper (a property of a tool's schema, a caller's own JSON) is data and stays.
fn canonical(value: &Value, out: &mut Writer<'_>, strip_marker: bool) -> Done {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map
                .keys()
                .filter(|k| !(strip_marker && k.as_str() == "cache_control"))
                .collect();
            keys.sort();
            put(out, b"{")?;
            for (n, key) in keys.into_iter().enumerate() {
                if n > 0 {
                    put(out, b",")?;
                }
                leaf(&Value::String(key.clone()), out)?;
                put(out, b":")?;
                canonical(&map[key], out, false)?;
            }
            put(out, b"}")
        }
        Value::Array(items) => {
            put(out, b"[")?;
            for (n, item) in items.iter().enumerate() {
                if n > 0 {
                    put(out, b",")?;
                }
                canonical(item, out, false)?;
            }
            put(out, b"]")
        }
        other => leaf(other, out),
    }
}

/// The canonical form of `map` without the keys `skip` names.
fn canonical_without(
    map: &serde_json::Map<String, Value>,
    skip: &[&str],
    out: &mut Writer<'_>,
) -> Done {
    let mut keys: Vec<&String> = map.keys().filter(|k| !skip.contains(&k.as_str())).collect();
    keys.sort();
    put(out, b"{")?;
    for (n, key) in keys.into_iter().enumerate() {
        if n > 0 {
            put(out, b",")?;
        }
        leaf(&Value::String(key.clone()), out)?;
        put(out, b":")?;
        canonical(&map[key], out, false)?;
    }
    put(out, b"}")
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

/// Fingerprints segments as they are walked, in order.
struct Collected<'k> {
    key: &'k [u8],
    out: Vec<Segment>,
    /// From the 2,048th segment on: one `other` segment standing for the rest, so a change anywhere in the tail
    /// still shows.
    tail: Option<(Mac, u64)>,
    spent: u64,
    over: bool,
}

impl<'k> Collected<'k> {
    fn new(key: &'k [u8]) -> Self {
        Self {
            key,
            out: Vec::new(),
            tail: None,
            spent: 0,
            over: false,
        }
    }

    /// One segment: `kind`, a zero byte, then what `write` puts out.
    fn emit(&mut self, kind: &'static str, write: impl FnOnce(&mut Writer<'_>) -> Done) {
        if self.over {
            return;
        }
        let folding = self.out.len() >= MAX_SEGMENTS - 1;
        let (mut mac, mut tail_len) = if folding {
            self.tail.take().unwrap_or_else(|| {
                let mut mac = Mac::new(self.key);
                mac.update(b"other\0");
                (mac, 0)
            })
        } else {
            (Mac::new(self.key), 0)
        };
        mac.update(kind.as_bytes());
        mac.update(&[0]);
        let mut writer = Writer {
            mac: &mut mac,
            len: 0,
            spent: &mut self.spent,
        };
        let finished = write(&mut writer);
        let len = writer.len;
        if finished.is_err() {
            self.over = true;
            return;
        }
        if folding {
            mac.update(&[0]);
            tail_len += len;
            self.tail = Some((mac, tail_len));
        } else {
            let hash = mac.finish()[..8]
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            self.out.push(Segment {
                kind,
                hash,
                bytes: len,
            });
        }
    }

    fn push(&mut self, kind: &'static str, value: &Value) {
        self.emit(kind, |w| canonical(value, w, true));
    }

    /// `{"<field>": value, "role": role}`, the fields in sorted order (every `field` we use sorts before `role`).
    fn push_pair(&mut self, kind: &'static str, field: &str, value: &Value, role: &str) {
        self.emit(kind, |w| {
            put(w, b"{")?;
            leaf(&Value::String(field.to_owned()), w)?;
            put(w, b":")?;
            canonical(value, w, true)?;
            put(w, b",\"role\":")?;
            leaf(&Value::String(role.to_owned()), w)?;
            put(w, b"}")
        });
    }

    /// A message's fields other than its content, as one segment.
    fn push_without(
        &mut self,
        kind: &'static str,
        map: &serde_json::Map<String, Value>,
        skip: &[&str],
    ) {
        self.emit(kind, |w| canonical_without(map, skip, w));
    }

    fn items(&mut self, kind: &'static str, value: Option<&Value>) {
        if let Some(Value::Array(items)) = value {
            for item in items {
                self.push(kind, item);
            }
        }
    }

    fn finish(mut self) -> Option<Vec<Segment>> {
        if self.over {
            return None;
        }
        if let Some((mac, len)) = self.tail.take() {
            let hash = mac.finish()[..8]
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            self.out.push(Segment {
                kind: "other",
                hash,
                bytes: len,
            });
        }
        Some(self.out)
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

fn anthropic(body: &Value, c: &mut Collected<'_>) {
    c.items("tool", body.get("tools"));
    match body.get("system") {
        Some(text @ Value::String(_)) => c.push("system", text),
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
            Some(text @ Value::String(_)) => c.push_pair(role_kind(role), "content", text, role),
            Some(Value::Array(blocks)) => {
                for block in blocks {
                    c.push_pair(block_kind(role, block), "block", block, role);
                }
            }
            _ => c.push(role_kind(role), message),
        }
    }
}

/// A chat-style message: an array content by its parts, then any other fields (tool calls, a tool call id) as one
/// more. A typed Responses item's `type` is not a field of its own.
fn chat_message(message: &Value, c: &mut Collected<'_>) {
    let role = message.get("role").and_then(Value::as_str).unwrap_or("");
    match (message.get("content"), message.as_object()) {
        (Some(Value::Array(parts)), Some(object)) => {
            for part in parts {
                c.push_pair(block_kind(role, part), "part", part, role);
            }
            let typed = object.get("type").and_then(Value::as_str) == Some("message");
            let skip: &[&str] = if typed {
                &["content", "type"]
            } else {
                &["content"]
            };
            if object
                .keys()
                .any(|k| k != "role" && !skip.contains(&k.as_str()))
            {
                c.push_without(role_kind(role), object, skip);
            }
        }
        _ => c.push(role_kind(role), message),
    }
}

fn openai_responses(body: &Value, c: &mut Collected<'_>) {
    c.items("tool", body.get("tools"));
    if let Some(text @ Value::String(_)) = body.get("instructions") {
        c.push("system", text);
    }
    match body.get("input") {
        Some(text @ Value::String(_)) => c.push_pair("user", "content", text, "user"),
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

fn chat_completions(body: &Value, c: &mut Collected<'_>) {
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

fn gemini(body: &Value, c: &mut Collected<'_>) {
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
            c.push_pair(kind, "part", part, role);
        }
    }
}

/// A wire's name as a payload document carries it, so a reader knows which walker cut the request.
pub fn wire_name(wire: WireFormat) -> &'static str {
    match wire {
        WireFormat::AnthropicMessages => "anthropic-messages",
        WireFormat::OpenAiResponses => "openai-responses",
        WireFormat::OpenAiChat => "openai-chat",
        WireFormat::GoogleGenerateContent => "google-generate-content",
    }
}

/// Why a body has no structure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skipped {
    /// Not a body this wire's walker understands (not an object, or none of the arrays the wire puts a prompt in).
    Unrecognized,
    /// Past the work budget ([`MAX_BYTES`]).
    TooLarge,
}

/// The segments of a provider-native request body. `key` is the deployment's capture secret.
pub fn segments(wire: WireFormat, body: &Value, key: &[u8]) -> Result<Vec<Segment>, Skipped> {
    let object = body.as_object().ok_or(Skipped::Unrecognized)?;
    let present = match wire {
        WireFormat::AnthropicMessages | WireFormat::OpenAiChat => object.contains_key("messages"),
        WireFormat::OpenAiResponses => object.contains_key("input"),
        WireFormat::GoogleGenerateContent => object.contains_key("contents"),
    };
    if !present {
        return Err(Skipped::Unrecognized);
    }
    let mut collected = Collected::new(key);
    match wire {
        WireFormat::AnthropicMessages => anthropic(body, &mut collected),
        WireFormat::OpenAiResponses => openai_responses(body, &mut collected),
        WireFormat::OpenAiChat => chat_completions(body, &mut collected),
        WireFormat::GoogleGenerateContent => gemini(body, &mut collected),
    }
    collected.finish().ok_or(Skipped::TooLarge)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
        assert!(segments(WireFormat::AnthropicMessages, &json!("text"), KEY).is_err());
        assert!(segments(WireFormat::OpenAiChat, &json!({"prompt": "x"}), KEY).is_err());
        assert!(segments(
            WireFormat::GoogleGenerateContent,
            &json!({"messages": []}),
            KEY
        )
        .is_err());
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

    #[test]
    fn only_a_cache_marker_is_stripped_and_a_caller_s_own_cache_control_is_data() {
        let body = |inner: &str| {
            json!({"messages": [{"role": "user", "content": "hi"}], "tools": [
            {"name": "f", "cache_control": {"type": "ephemeral"}, "input_schema": {"properties": {"cache_control": {"type": inner}}}}]})
        };
        let a = segments(WireFormat::AnthropicMessages, &body("string"), KEY).unwrap();
        let b = segments(WireFormat::AnthropicMessages, &body("number"), KEY).unwrap();
        assert_ne!(
            a[0].hash, b[0].hash,
            "a change deep in a tool schema must show"
        );
        // The marker on the tool itself does not.
        let mut marked = body("string");
        marked["tools"][0]
            .as_object_mut()
            .unwrap()
            .remove("cache_control");
        assert_eq!(
            segments(WireFormat::AnthropicMessages, &marked, KEY).unwrap(),
            a
        );
    }

    #[test]
    fn a_typed_responses_message_is_its_parts_and_no_spurious_segment() {
        let body = json!({"input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "a"}, {"type": "input_text", "text": "b"}]}]});
        assert_eq!(
            kinds(&segments(WireFormat::OpenAiResponses, &body, KEY).unwrap()),
            ["user", "user"]
        );
        // A field beside the content is still a segment of its own.
        let named = json!({"messages": [{"role": "user", "name": "bob", "content": [{"type": "text", "text": "a"}]}]});
        assert_eq!(
            kinds(&segments(WireFormat::OpenAiChat, &named, KEY).unwrap()),
            ["user", "user"]
        );
    }

    #[test]
    fn a_request_past_the_work_budget_has_no_structure_instead_of_costing_the_request() {
        let big = "x".repeat((MAX_BYTES as usize) + 10);
        let body = json!({"messages": [{"role": "user", "content": [{"type": "image_url", "image_url": {"url": big}}]}]});
        assert!(segments(WireFormat::OpenAiChat, &body, KEY).is_err());
        // Just under the budget is hashed as usual.
        let fits = "x".repeat(1 << 20);
        let body = json!({"messages": [{"role": "user", "content": [{"type": "image_url", "image_url": {"url": fits}}]}]});
        let s = segments(WireFormat::OpenAiChat, &body, KEY).unwrap();
        assert!(s[0].bytes > (1 << 20));
    }
}
