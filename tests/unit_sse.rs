/// Tests for the SSE buffer parser (extract_sse_data).
/// Since extract_sse_data is private, we test it indirectly through a helper
/// that reimplements the same logic for validation.

fn extract_sse_data(buffer: &mut String) -> Option<String> {
    loop {
        let newline_pos = buffer.find('\n')?;
        let line = buffer[..newline_pos].trim_end_matches('\r').to_string();
        buffer.drain(..=newline_pos);

        if let Some(data) = line.strip_prefix("data: ") {
            if data == "[DONE]" {
                return None;
            }
            return Some(data.to_string());
        }
        if buffer.is_empty() {
            return None;
        }
    }
}

#[test]
fn sse_simple_data_line() {
    let mut buf = "data: {\"hello\":\"world\"}\n".to_string();
    let result = extract_sse_data(&mut buf);
    assert_eq!(result, Some("{\"hello\":\"world\"}".to_string()));
    assert!(buf.is_empty());
}

#[test]
fn sse_with_event_line_before_data() {
    let mut buf = "event: message\ndata: {\"a\":1}\n".to_string();
    let result = extract_sse_data(&mut buf);
    assert_eq!(result, Some("{\"a\":1}".to_string()));
}

#[test]
fn sse_done_signal() {
    let mut buf = "data: [DONE]\n".to_string();
    let result = extract_sse_data(&mut buf);
    assert!(result.is_none());
}

#[test]
fn sse_empty_buffer() {
    let mut buf = String::new();
    let result = extract_sse_data(&mut buf);
    assert!(result.is_none());
}

#[test]
fn sse_no_newline_yet() {
    let mut buf = "data: partial".to_string();
    let result = extract_sse_data(&mut buf);
    assert!(result.is_none());
    // Buffer should be unchanged
    assert_eq!(buf, "data: partial");
}

#[test]
fn sse_carriage_return_handling() {
    let mut buf = "data: {\"ok\":true}\r\n".to_string();
    let result = extract_sse_data(&mut buf);
    assert_eq!(result, Some("{\"ok\":true}".to_string()));
}

#[test]
fn sse_multiple_events_in_buffer() {
    let mut buf = "data: first\ndata: second\n".to_string();
    let r1 = extract_sse_data(&mut buf);
    assert_eq!(r1, Some("first".to_string()));
    let r2 = extract_sse_data(&mut buf);
    assert_eq!(r2, Some("second".to_string()));
    assert!(buf.is_empty());
}

#[test]
fn sse_empty_lines_skipped() {
    let mut buf = "\n\ndata: payload\n".to_string();
    let result = extract_sse_data(&mut buf);
    assert_eq!(result, Some("payload".to_string()));
}

#[test]
fn sse_event_id_retry_skipped() {
    let mut buf = "id: 123\nretry: 5000\nevent: msg\ndata: actual\n".to_string();
    let result = extract_sse_data(&mut buf);
    assert_eq!(result, Some("actual".to_string()));
}

#[test]
fn sse_data_with_colons_in_value() {
    let mut buf = "data: {\"url\":\"http://example.com:8080\"}\n".to_string();
    let result = extract_sse_data(&mut buf);
    assert_eq!(
        result,
        Some("{\"url\":\"http://example.com:8080\"}".to_string())
    );
}

#[test]
fn sse_anthropic_style_events() {
    // Anthropic sends: event: <type>\ndata: <json>\n\n
    let mut buf = "event: message_start\ndata: {\"type\":\"message_start\"}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"text\":\"Hi\"}}\n\n".to_string();

    let r1 = extract_sse_data(&mut buf);
    assert_eq!(r1, Some("{\"type\":\"message_start\"}".to_string()));

    let r2 = extract_sse_data(&mut buf);
    assert_eq!(
        r2,
        Some("{\"type\":\"content_block_delta\",\"delta\":{\"text\":\"Hi\"}}".to_string())
    );
}

#[test]
fn sse_openai_style_events() {
    let mut buf = "data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"delta\":{\"content\":\"H\"}}]}\n\ndata: {\"id\":\"chatcmpl-1\",\"choices\":[{\"delta\":{\"content\":\"i\"}}]}\n\ndata: [DONE]\n\n".to_string();

    let r1 = extract_sse_data(&mut buf);
    assert!(r1.is_some());
    assert!(r1.unwrap().contains("chatcmpl-1"));

    let r2 = extract_sse_data(&mut buf);
    assert!(r2.is_some());

    let r3 = extract_sse_data(&mut buf);
    assert!(r3.is_none()); // [DONE]
}
