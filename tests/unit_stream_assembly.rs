use llmshim::reasoning::ReasoningAccumulator;
use serde_json::{json, Value};

#[test]
fn fragmented_reasoning_preserves_fields_snapshots_and_origin() {
    let original_origin = json!({"provider": "test", "model": "original"});
    let mut accumulator = ReasoningAccumulator::default();
    accumulator
        .push(&json!({"reasoning": [{
            "index": 0,
            "origin": original_origin,
            "text": null,
            "signature": 0,
            "payload": {"thinking": false, "summary": null, "other": 0}
        }]}))
        .unwrap();
    for sequence in 0..64 {
        accumulator
            .push(&json!({"reasoning": [{
            "index": 0,
            "origin": {"provider": "test", "model": "untrusted-change"},
            "text": "λ",
            "signature": "s",
            "data": "d",
            "payload": {
                "text": "λ",
                "signature": "s",
                "data": "d",
                "thinking": "t",
                "summary": "μ",
                "other": sequence
            }
        }, {"index": 1, "text": "β"}]}))
            .unwrap();
    }
    let assembled = accumulator.blocks();
    assert_eq!(assembled.len(), 2);
    assert_eq!(assembled[0]["text"], "λ".repeat(64));
    assert_eq!(assembled[0]["signature"], "s".repeat(64));
    assert_eq!(assembled[0]["data"], "d".repeat(64));
    for (field, fragment) in [
        ("text", "λ"),
        ("signature", "s"),
        ("data", "d"),
        ("thinking", "t"),
        ("summary", "μ"),
    ] {
        assert_eq!(assembled[0]["payload"][field], fragment.repeat(64));
    }
    assert_eq!(assembled[0]["payload"]["other"], 63);
    assert_eq!(assembled[0]["origin"], original_origin);
    assert_eq!(assembled[1]["text"], "β".repeat(64));

    accumulator
        .push(&json!({"reasoning": [{
            "index": 0,
            "replace": true,
            "origin": {"provider": "test", "model": "untrusted-change"},
            "text": "complete",
            "signature": "final"
        }]}))
        .unwrap();
    let completed = accumulator.blocks();
    assert_eq!(completed[0]["text"], "complete");
    assert_eq!(completed[0]["signature"], "final");
    assert_eq!(completed[0]["origin"], original_origin);
    assert!(completed[0].get("data").is_none());
    assert_eq!(completed[1], assembled[1]);
}

#[tokio::test]
async fn collected_fragments_keep_choices_content_and_refusals_separate() {
    let mut chunks = Vec::new();
    for _ in 0..64 {
        chunks.push(Ok(json!({
            "model": "test/model",
            "choices": [
                {"index": 0, "delta": {"content": "λ", "refusal": "μ"}},
                {"index": 1, "delta": {"content": "β", "refusal": "γ"}}
            ]
        })
        .to_string()));
    }
    chunks.push(Ok(json!({"choices": [
        {"index": 0, "delta": {"content": null, "refusal": null}, "finish_reason": "stop"},
        {"index": 1, "delta": {"content": "", "refusal": false}, "finish_reason": "content_filter"}
    ]})
    .to_string()));
    let response = llmshim::shim::collect(Box::pin(futures::stream::iter(chunks)))
        .await
        .unwrap();
    assert_eq!(response["model"], "test/model");
    let choices = response["choices"].as_array().unwrap();
    assert_eq!(choices.len(), 2);
    for (index, content, refusal, finish) in
        [(0, "λ", "μ", "stop"), (1, "β", "γ", "content_filter")]
    {
        let choice: &Value = &choices[index];
        assert_eq!(choice["index"], index);
        assert_eq!(choice["message"]["content"], content.repeat(64));
        assert_eq!(choice["message"]["refusal"], refusal.repeat(64));
        assert_eq!(choice["finish_reason"], finish);
    }
}
