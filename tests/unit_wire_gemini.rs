#![cfg(feature = "proxy")]
//! The native Gemini wire's inbound facade: its own paths, its own shapes and
//! its own error object. `unit_wire.rs` holds the cases the two older wires
//! share, plus the cross-wire tables this wire's rows extend.

use axum::{
    body::{to_bytes, Body},
    http::Request,
    Extension,
};
use llmshim::{
    provider::Provider,
    providers::gemini::Gemini,
    proxy::{
        app,
        wire::{native_route, request_to_chat, response_from_chat, NativeRoute, Receipts, Wire},
    },
    router::Router,
};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

#[path = "support/native_post.rs"]
mod native_post;
use native_post::post;

/// A canonical response whose reasoning block came from the Messages wire, so
/// this wire may only hand it back as an opaque handle. Written out rather than
/// derived from the Anthropic adapter, so the fixture does not depend on
/// another wire's normalizer.
fn foreign_reasoning() -> Value {
    json!({
        "id": "response-a",
        "model": "gemini/gemini-3.8-flash",
        "message": {
            "role": "assistant",
            "content": Value::Null,
            "reasoning": [{
                "kind": "text",
                "text": "brief",
                "signature": "opaque",
                "origin": {
                    "provider": "anthropic",
                    "model": "claude-sonnet-4-6",
                    "family": "claude",
                    "wire": "anthropic-messages",
                    "received_at": "2026-09-16T00:00:00Z",
                },
            }],
            "tool_calls": [{
                "id": "call_ls_a",
                "type": "function",
                "function": {"name": "read", "arguments": "{\"path\":\"a\"}"},
            }],
        },
        "finish_reason": "tool_calls",
        "usage": {"input_tokens": 4, "output_tokens": 2, "total_tokens": 6},
    })
}

const GEMINI_SIGNATURE: &str = "opaque-thought-signature";

const GEMINI_PATH: &str = "/v1beta/models/gemini/gemini-3.8-flash:generateContent";

fn gemini_router(base_url: String) -> Router {
    Router::new().register(
        "gemini",
        Box::new(Gemini::new("test-key".into()).with_base_url(base_url)),
    )
}

#[test]
fn gemini_tools_and_calling_modes_become_canonical_ones() {
    let dir = tempfile::tempdir().unwrap();
    let store = Receipts::new(dir.path().to_owned());
    let canonical = |extra: Value| {
        let mut request = json!({
            "model": "gemini/gemini-3.8-flash",
            "contents": [{"role":"user","parts":[{"text":"read"}]}],
        });
        for (key, value) in extra.as_object().unwrap() {
            request[key] = value.clone();
        }
        request_to_chat(&request, Wire::Gemini, &store, "a").unwrap()["provider_config"].clone()
    };
    let declared = json!({
        "tools": [{"functionDeclarations": [{
            "name": "read",
            "description": "Read a file",
            "parameters": {"type":"object","properties":{"path":{"type":"string"}}},
        }]}],
    });
    let config = canonical(declared.clone());
    assert_eq!(
        config["tools"],
        json!([{"type":"function","function":{
            "name": "read",
            "description": "Read a file",
            "parameters": {"type":"object","properties":{"path":{"type":"string"}}},
        }}])
    );
    // A declaration without parameters still needs the object-tool shape.
    let bare = canonical(json!({"tools": [{"functionDeclarations": [{"name": "ping"}]}]}));
    assert_eq!(
        bare["tools"][0]["function"]["parameters"],
        json!({"type":"object","properties":{}})
    );
    // Google's three calling modes are the canonical three.
    for (mode, expected) in [("AUTO", "auto"), ("ANY", "required"), ("NONE", "none")] {
        let config = canonical(json!({"toolConfig": {"functionCallingConfig": {"mode": mode}}}));
        assert_eq!(config["tool_choice"], expected, "{mode}");
    }
}

#[test]
fn gemini_a_schema_decides_structured_output_over_the_mime_type() {
    let dir = tempfile::tempdir().unwrap();
    let store = Receipts::new(dir.path().to_owned());
    let format = |generation: Value| {
        request_to_chat(
            &json!({
                "model": "gemini/gemini-3.8-flash",
                "contents": [{"role":"user","parts":[{"text":"hi"}]}],
                "generationConfig": generation,
            }),
            Wire::Gemini,
            &store,
            "a",
        )
        .unwrap()["provider_config"]["response_format"]
            .clone()
    };
    let schema = json!({"type":"object","properties":{"answer":{"type":"string"}}});
    // A schema alone, and a schema beside the mime type in either order: the
    // schema decides, and no key order may pick the weaker shape.
    assert_eq!(
        format(json!({"responseSchema": schema}))["type"],
        "json_schema"
    );
    assert_eq!(
        format(json!({"responseMimeType":"application/json","responseSchema": schema}))["type"],
        "json_schema"
    );
    assert_eq!(
        format(json!({"responseSchema": schema,"responseMimeType":"application/json"}))["type"],
        "json_schema"
    );
    assert_eq!(
        format(json!({"responseSchema": schema}))["json_schema"]["schema"],
        schema
    );
    // Without one, `application/json` is bare object mode, and the default text
    // mime type asks for nothing at all.
    assert_eq!(
        format(json!({"responseMimeType":"application/json"})),
        json!({"type":"json_object"})
    );
    assert!(format(json!({"responseMimeType":"text/plain"})).is_null());
}

#[test]
fn gemini_media_parts_accept_googles_two_spellings() {
    let dir = tempfile::tempdir().unwrap();
    let store = Receipts::new(dir.path().to_owned());
    let block = |part: Value| {
        request_to_chat(
            &json!({
                "model": "gemini/gemini-3.8-flash",
                "contents": [{"role":"user","parts":[part]}],
            }),
            Wire::Gemini,
            &store,
            "a",
        )
        .unwrap()["messages"][0]["content"][0]
            .clone()
    };
    // Protobuf JSON accepts either spelling of a field name and Google's SDKs
    // send one or the other, so both have to read the same.
    for part in [
        json!({"inlineData":{"mimeType":"image/png","data":"AAAA"}}),
        json!({"inline_data":{"mime_type":"image/png","data":"AAAA"}}),
    ] {
        assert_eq!(
            block(part)["image_url"]["url"],
            "data:image/png;base64,AAAA"
        );
    }
    // A file Google holds has no canonical image block, so it becomes the text
    // note the outbound adapter already uses for a remote image URL.
    for part in [
        json!({"fileData":{"fileUri":"files/abc","mimeType":"image/png"}}),
        json!({"file_data":{"file_uri":"files/abc","mime_type":"image/png"}}),
    ] {
        assert_eq!(block(part)["text"], "[Image: files/abc]");
    }
}

#[test]
fn gemini_paths_route_only_the_two_generate_actions() {
    let gemini = |model: &str, streams| NativeRoute {
        wire: Wire::Gemini,
        model: Some(model.to_owned()),
        streams,
    };
    assert_eq!(
        native_route("/v1beta/models/gemini/gemini-3.8-flash:generateContent"),
        Some(gemini("gemini/gemini-3.8-flash", false))
    );
    assert_eq!(
        native_route("/v1beta/models/gemini-3.5-flash-lite:streamGenerateContent"),
        Some(gemini("gemini-3.5-flash-lite", true))
    );
    assert_eq!(
        native_route("/v1/messages"),
        Some(NativeRoute {
            wire: Wire::Messages,
            model: None,
            streams: false,
        })
    );
    assert_eq!(
        native_route("/v1/chat/completions"),
        Some(NativeRoute {
            wire: Wire::Chat,
            model: None,
            streams: false,
        })
    );
    // Everything else is not a native route: a wrong action, a wrong verb, a
    // model that is missing or carries an extra colon, and a sibling prefix.
    for path in [
        "/v1beta/models/gemini-3.8-flash:countTokens",
        "/v1beta/models/gemini-3.8-flash:generatecontent",
        "/v1beta/models/gemini-3.8-flash:streamGenerateContentX",
        "/v1beta/models/:generateContent",
        "/v1beta/models/a:b:generateContent",
        "/v1beta/models/gemini-3.8-flash",
        "/v1beta/models/gemini-3.8-flash:generateContent/extra",
        "/v1beta/other/models/gemini-3.8-flash:generateContent",
        "/v1/chat",
    ] {
        assert!(native_route(path).is_none(), "{path} is not a native route");
    }
}

#[tokio::test]
async fn gemini_paths_that_name_no_served_action_are_refused_in_googles_shape() {
    // The path is routed by a wildcard, so an unserved action would otherwise
    // fall through to the chat handler: with a Gemini body that is an axum
    // deserialization rejection in `text/plain`, and with a chat-shaped body it
    // silently runs a completion nobody asked for.
    for (path, body) in [
        (
            "/v1beta/models/gemini/gemini-3.8-flash:countTokens",
            json!({"contents":[{"role":"user","parts":[{"text":"hi"}]}]}),
        ),
        (
            "/v1beta/models/gemini/gemini-3.8-flash:generatecontent",
            json!({"contents":[{"role":"user","parts":[{"text":"hi"}]}]}),
        ),
        (
            "/v1beta/models/gemini/gemini-3.8-flash",
            json!({"contents":[{"role":"user","parts":[{"text":"hi"}]}]}),
        ),
        (
            "/v1beta/models/gemini/gemini-3.8-flash:embedContent",
            json!({"model":"local/test","messages":[{"role":"user","content":"hi"}]}),
        ),
    ] {
        let (status, response) = post(app(Router::new(), None), path, body).await;
        assert_eq!(status, 404, "{path}: {response}");
        assert_eq!(response["error"]["code"], 404, "{path}");
        assert_eq!(response["error"]["status"], "NOT_FOUND", "{path}");
        assert!(
            response["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("generateContent")),
            "{path}: {response}"
        );
    }
}

#[tokio::test]
async fn gemini_native_endpoint_serves_generate_content_with_its_own_shapes() {
    let mut server = mockito::Server::new_async().await;
    let upstream = server
        .mock("POST", "/models/gemini-3.8-flash:generateContent")
        .match_query(mockito::Matcher::UrlEncoded(
            "key".into(),
            "test-key".into(),
        ))
        .match_body(mockito::Matcher::PartialJson(json!({
            "contents": [{"role":"user","parts":[{"text":"hi"}]}],
            "systemInstruction": {"parts":[{"text":"be brief"}]},
            "cachedContent": "cached/1",
            "generationConfig": {
                "topP": 0.5,
                "maxOutputTokens": 64,
                "thinkingConfig": {"includeThoughts": true, "thinkingLevel": "high"},
            },
        })))
        .with_body(
            json!({
                "candidates": [{
                    "content": {"role":"model","parts":[{"text":"hello"}]},
                    "finishReason": "STOP",
                    "index": 0,
                }],
                "usageMetadata": {
                    "promptTokenCount": 4,
                    "candidatesTokenCount": 2,
                    "totalTokenCount": 6,
                    "cachedContentTokenCount": 3,
                },
                "modelVersion": "gemini-3.8-flash",
                "responseId": "response-1",
            })
            .to_string(),
        )
        .expect(1)
        .create_async()
        .await;
    let dir = tempfile::tempdir().unwrap();
    let (status, body) = post(
        app(gemini_router(server.url()), None)
            .layer(Extension(Arc::new(Receipts::new(dir.path().to_owned())))),
        GEMINI_PATH,
        json!({
            "contents": [{"role":"user","parts":[{"text":"hi"}]}],
            "systemInstruction": {"parts":[{"text":"be brief"}]},
            "cachedContent": "cached/1",
            "generationConfig": {
                "topP": 0.5,
                "maxOutputTokens": 64,
                "thinkingConfig": {"thinkingLevel": "high"},
            },
        }),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body["candidates"][0]["content"]["parts"][0]["text"],
        "hello"
    );
    assert_eq!(body["candidates"][0]["content"]["role"], "model");
    assert_eq!(body["candidates"][0]["finishReason"], "STOP");
    // Gemini names its token counts; the routing id stays what the caller asked.
    assert_eq!(body["usageMetadata"]["promptTokenCount"], 4);
    assert_eq!(body["usageMetadata"]["cachedContentTokenCount"], 3);
    assert_eq!(body["modelVersion"], "gemini-3.8-flash");
    assert_eq!(body["responseId"], "response-1");
    upstream.assert_async().await;
}

#[tokio::test]
async fn gemini_native_stream_ends_with_a_finish_reason_and_no_done_marker() {
    let mut server = mockito::Server::new_async().await;
    let data = format!(
        "data: {}\n\ndata: {}\n\n",
        json!({"candidates":[{"content":{"role":"model","parts":[
            {"thought":true,"text":"thinking","thoughtSignature":GEMINI_SIGNATURE},
            {"text":"hello"},
        ]},"index":0}]}),
        json!({
            "candidates":[{"content":{"role":"model","parts":[]},"finishReason":"STOP","index":0}],
            "usageMetadata":{"promptTokenCount":4,"candidatesTokenCount":2,"totalTokenCount":6},
            "modelVersion":"gemini-3.8-flash",
        })
    );
    let upstream = server
        .mock("POST", "/models/gemini-3.8-flash:streamGenerateContent")
        .match_query(mockito::Matcher::Any)
        .with_header("content-type", "text/event-stream")
        .with_body(data)
        .expect(1)
        .create_async()
        .await;
    let dir = tempfile::tempdir().unwrap();
    let application = app(gemini_router(server.url()), None)
        .layer(Extension(Arc::new(Receipts::new(dir.path().to_owned()))));
    let response = application
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1beta/models/gemini/gemini-3.8-flash:streamGenerateContent")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"contents":[{"role":"user","parts":[{"text":"hi"}]}]}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert!(response.headers()["content-type"]
        .to_str()
        .unwrap()
        .contains("text/event-stream"));
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    upstream.assert_async().await;
    assert!(text.contains(r#"{"text":"hello"}"#), "{text}");
    // A thought part carries text too; only the answer's own text was already
    // streamed, so reasoning has to survive this tail intact - once.
    assert!(text.contains(r#""thought":true"#), "{text}");
    assert!(
        text.contains(&format!(r#""thoughtSignature":"{GEMINI_SIGNATURE}""#)),
        "{text}"
    );
    assert_eq!(text.matches(r#"{"text":"hello"}"#).count(), 1, "{text}");
    assert!(text.contains(r#""finishReason":"STOP""#), "{text}");
    assert!(text.contains(r#""promptTokenCount":4"#), "{text}");
    // Gemini's SSE carries no event names and no terminator of its own, and
    // nothing on it may be shaped like another wire's chunk.
    assert!(!text.contains("event: "), "{text}");
    assert!(!text.contains("[DONE]"), "{text}");
    assert!(!text.contains("chat.completion"), "{text}");
}

#[tokio::test]
async fn gemini_refuses_what_it_cannot_express_and_names_it() {
    // Each of these is a field Gemini accepts and the canonical request cannot
    // carry, so forwarding it would either fail upstream or disappear.
    for (extra, named) in [
        (json!({"cachedContents": "cached/1"}), "cachedContents"),
        // The engine applies explicit cache markers on the Anthropic wire only
        // (`cache::prepare_request` returns every other wire untouched), so
        // accepting them here would promise a boundary nothing acts on. They
        // remain available on `/v1/messages`.
        (
            json!({"x-cache": {"segments": [{"upto_message": 0, "stability": "session"}]}}),
            "x-cache",
        ),
        (
            json!({"generationConfig": {"thinkingBudget": 512}}),
            "thinkingBudget",
        ),
        (
            json!({"generationConfig": {"candidateCount": 2}}),
            "candidateCount",
        ),
        (
            json!({"generationConfig": {"thinkingConfig": {"includeThoughts": false}}}),
            "includeThoughts",
        ),
        (
            json!({"toolConfig": {"functionCallingConfig": {
                "mode": "ANY", "allowedFunctionNames": ["read"]
            }}}),
            "allowedFunctionNames",
        ),
        (
            json!({"toolConfig": {"functionCallingConfig": {"mode": "NEVER"}}}),
            "unsupported function calling mode",
        ),
        (
            json!({"generationConfig": {"responseMimeType": "text/html"}}),
            "responseMimeType",
        ),
        (
            json!({"tools": [{"googleSearch": {}}]}),
            "custom function tools",
        ),
        (
            json!({"contents": [{"role": "user", "parts": [{"executableCode": {}}]}]}),
            "unsupported Gemini content part",
        ),
        (
            json!({"systemInstruction": {"parts": [{"inlineData": {}}]}}),
            "systemInstruction supports text parts only",
        ),
    ] {
        let mut request = extra;
        request["contents"] = request
            .get("contents")
            .cloned()
            .unwrap_or_else(|| json!([{"role":"user","parts":[{"text":"hi"}]}]));
        let (status, body) = post(app(Router::new(), None), GEMINI_PATH, request).await;
        assert_eq!(status, 400, "{named}: {body}");
        assert_eq!(body["error"]["code"], 400);
        assert_eq!(body["error"]["status"], "INVALID_ARGUMENT");
        assert!(
            body["error"]["message"].as_str().unwrap().contains(named),
            "{named}: {body}"
        );
    }
}

#[tokio::test]
async fn gemini_finish_reasons_keep_googles_vocabulary() {
    // An engine finish reason is not a Gemini one: a truncated answer is
    // MAX_TOKENS here and a filtered one is SAFETY, and neither survives a
    // mapping that falls back to STOP.
    for reason in ["MAX_TOKENS", "SAFETY"] {
        let mut server = mockito::Server::new_async().await;
        let upstream = server
            .mock("POST", "/models/gemini-3.8-flash:generateContent")
            .match_query(mockito::Matcher::Any)
            .with_body(
                json!({
                    "candidates": [{
                        "content": {"role":"model","parts":[{"text":"hi"}]},
                        "finishReason": reason,
                        "index": 0,
                    }],
                    "usageMetadata": {},
                })
                .to_string(),
            )
            .expect(1)
            .create_async()
            .await;
        let dir = tempfile::tempdir().unwrap();
        let (status, body) = post(
            app(gemini_router(server.url()), None)
                .layer(Extension(Arc::new(Receipts::new(dir.path().to_owned())))),
            GEMINI_PATH,
            json!({"contents":[{"role":"user","parts":[{"text":"hi"}]}]}),
        )
        .await;
        assert_eq!(status, 200, "{reason}: {body}");
        assert_eq!(body["candidates"][0]["finishReason"], reason);
        upstream.assert_async().await;
    }
}

#[test]
fn gemini_tool_signature_and_call_id_survive_a_receipted_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let store = Receipts::new(dir.path().to_owned());
    // The adapter stamps the signature Google sent on the part beside the call.
    let normalized = Gemini::new("test-key".into())
        .transform_response(
            "gemini-3.8-flash",
            json!({
                "candidates": [{
                    "content": {"role":"model","parts":[{
                        "functionCall": {"name":"read","args":{"path":"a"},"id":"wire-1"},
                        "thoughtSignature": GEMINI_SIGNATURE,
                    }]},
                    "finishReason": "STOP",
                    "index": 0,
                }],
                "usageMetadata": {"promptTokenCount":4,"candidatesTokenCount":2,"totalTokenCount":6},
            }),
        )
        .unwrap();
    let canonical = json!({
        "id": "response-1",
        "model": "gemini/gemini-3.8-flash",
        "message": normalized["choices"][0]["message"],
        "finish_reason": "tool_calls",
        "usage": {"input_tokens":4,"output_tokens":2,"total_tokens":6},
    });
    let call_id = canonical["message"]["tool_calls"][0]["id"].clone();
    let exported = response_from_chat(&canonical, Wire::Gemini, &store, "a").unwrap();
    let part = exported["candidates"][0]["content"]["parts"][0].clone();
    assert_eq!(part["thoughtSignature"], GEMINI_SIGNATURE);
    assert_eq!(part["functionCall"]["id"], call_id);
    // Google lets the caller drop a response's id, and pairs it by name.
    let request = json!({
        "model": "gemini/gemini-3.8-flash",
        "contents": [
            {"role":"user","parts":[{"text":"read"}]},
            {"role":"model","parts":[part]},
            {"role":"user","parts":[{"functionResponse":{"name":"read","response":{"ok":true}}}]},
        ],
    });
    let imported = request_to_chat(&request, Wire::Gemini, &store, "a").unwrap();
    assert_eq!(imported["messages"][1]["tool_calls"][0]["id"], call_id);
    assert_eq!(
        imported["messages"][1]["tool_calls"][0]["thought_signature"]["data"],
        GEMINI_SIGNATURE
    );
    assert_eq!(imported["messages"][2]["tool_call_id"], call_id);
    // ...and the Gemini adapter puts the original wire id and signature back.
    let upstream = Gemini::new("test-key".into())
        .transform_request(
            "gemini-3.8-flash",
            &json!({"messages": imported["messages"]}),
        )
        .unwrap();
    assert_eq!(
        upstream.body["contents"][1]["parts"][0]["functionCall"]["id"],
        "wire-1"
    );
    assert_eq!(
        upstream.body["contents"][1]["parts"][0]["thoughtSignature"],
        GEMINI_SIGNATURE
    );
    // Another credential cannot restore this call: an owned id without its
    // receipt is an error, never a silently different request.
    let other = Receipts::new(dir.path().to_owned());
    assert!(request_to_chat(&request, Wire::Gemini, &other, "b").is_err());
    // Google pairs an id-less response by name, and a name with no call to
    // answer is not a history this wire may invent a pairing for.
    assert!(request_to_chat(
        &json!({
            "model": "gemini/gemini-3.8-flash",
            "contents": [{"role":"user","parts":[
                {"functionResponse":{"name":"ghost","response":{}}}
            ]}],
        }),
        Wire::Gemini,
        &store,
        "a"
    )
    .is_err());
}

#[test]
fn gemini_thought_parts_are_restored_through_their_receipt() {
    let dir = tempfile::tempdir().unwrap();
    let store = Receipts::new(dir.path().to_owned());
    // The block below came from the Messages wire, so its signature is not
    // Gemini's to present: the part carries a handle instead.
    let canonical = foreign_reasoning();
    let exported = response_from_chat(&canonical, Wire::Gemini, &store, "a").unwrap();
    let parts = exported["candidates"][0]["content"]["parts"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(parts[0]["thought"], true);
    assert_eq!(parts[0]["text"], "brief");
    let handle = parts[0]["thoughtSignature"].as_str().unwrap();
    assert!(handle.starts_with("lsr_"), "{handle}");

    // A block this wire issued keeps its own signature instead, so a client can
    // replay it without depending on the receipt directory.
    let ours = Gemini::new("test-key".into())
        .transform_response(
            "gemini-3.8-flash",
            json!({
                "candidates": [{
                    "content": {"role":"model","parts":[
                        {"text":"reasoning here","thought":true,"thoughtSignature":GEMINI_SIGNATURE},
                        {"text":"answer"},
                    ]},
                    "finishReason": "STOP",
                    "index": 0,
                }],
                "usageMetadata": {},
            }),
        )
        .unwrap();
    let issued = json!({
        "id": "response-2",
        "model": "gemini/gemini-3.8-flash",
        "message": ours["choices"][0]["message"],
        "finish_reason": "stop",
        "usage": {},
    });
    let exported = response_from_chat(&issued, Wire::Gemini, &store, "a").unwrap();
    assert_eq!(
        exported["candidates"][0]["content"]["parts"][0]["thoughtSignature"],
        GEMINI_SIGNATURE
    );

    let echoed = json!({
        "model": "gemini/gemini-3.8-flash",
        "contents": [
            {"role":"user","parts":[{"text":"read"}]},
            {"role":"model","parts":parts},
            {"role":"user","parts":[{"functionResponse":{"name":"read","response":{"ok":true}}}]},
        ],
    });
    let imported = request_to_chat(&echoed, Wire::Gemini, &store, "a").unwrap();
    assert_eq!(
        imported["messages"][1]["reasoning"],
        canonical["message"]["reasoning"]
    );
    // With the receipt gone the thought is dropped, never re-attributed to
    // whichever provider this request happens to reach.
    let thought_only = json!({
        "model": "gemini/gemini-3.8-flash",
        "contents": [
            {"role":"user","parts":[{"text":"read"}]},
            {"role":"model","parts":[{"thought":true,"text":"brief","thoughtSignature":handle}]},
        ],
    });
    let orphaned = request_to_chat(
        &thought_only,
        Wire::Gemini,
        &Receipts::new(dir.path().to_owned()),
        "b",
    )
    .unwrap();
    assert_eq!(orphaned["messages"].as_array().unwrap().len(), 1);
}
