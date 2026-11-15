use hive_agent_core::provider::ModelEvent;
use hive_agent_core::provider::streaming::{Accumulator, SseDecoder};
use serde_json::json;

fn text(value: &str) -> serde_json::Value {
    json!({"choices":[{"index":0,"delta":{"content":value},"finish_reason":null}]})
}
fn finish(reason: &str) -> serde_json::Value {
    json!({"choices":[{"index":0,"delta":{},"finish_reason":reason}]})
}

#[test]
fn sse_handles_utf8_across_every_byte_boundary_and_all_line_endings() {
    let mut decoder = SseDecoder::default();
    let input =
        "\u{feff}:heartbeat\r\ndata: {\"text\":\r\ndata: \"niño 🐝\"}\r\n\r\ndata: [DONE]\n\n";
    let mut frames = vec![];
    for byte in input.as_bytes() {
        frames.extend(decoder.feed(&[*byte]).unwrap());
    }
    assert_eq!(frames, vec!["{\"text\":\n\"niño 🐝\"}", "[DONE]"]);
    assert_eq!(decoder.feed(b"data: x\r\r").unwrap(), vec!["x"]);
}

#[test]
fn invalid_utf8_and_oversized_sse_frames_fail() {
    assert!(SseDecoder::default().feed(b"data: \xff\n\n").is_err());
    assert!(
        SseDecoder::default()
            .feed(&vec![b'x'; 1024 * 1024 + 1])
            .is_err()
    );
}

#[test]
fn text_and_usage_assemble_without_double_counting_usage() {
    let mut acc = Accumulator::default();
    assert!(
        matches!(acc.push(text("Hola ")).unwrap().as_slice(),[ModelEvent::TextDelta {text}] if text=="Hola ")
    );
    acc.push(text("🐝")).unwrap();
    acc.push(finish("stop")).unwrap();
    let events = acc
        .push(json!({"choices":[],"usage":{"prompt_tokens":7,"completion_tokens":2}}))
        .unwrap();
    assert!(matches!(events.as_slice(),[ModelEvent::Usage {usage}] if usage.input_tokens==7));
    let reply = acc.finish().unwrap();
    assert_eq!(reply.content.as_deref(), Some("Hola 🐝"));
    assert_eq!(reply.usage.unwrap().output_tokens, 2);
}

#[test]
fn interleaved_tool_deltas_assemble_json_only_at_finish() {
    let mut acc = Accumulator::default();
    acc.push(json!({"choices":[{"delta":{"tool_calls":[
        {"index":1,"id":"b","type":"function","function":{"name":"lookup","arguments":"{\"q\":"}},
        {"index":0,"id":"a","function":{"name":"ad","arguments":"{\"a\":2,"}}
    ]}}]}))
    .unwrap();
    acc.push(json!({"choices":[{"delta":{"tool_calls":[
        {"index":0,"function":{"name":"d","arguments":"\"b\":3}"}},
        {"index":1,"function":{"arguments":"\"hi\"}"}}
    ]}}]}))
    .unwrap();
    acc.push(finish("tool_calls")).unwrap();
    let reply = acc.finish().unwrap();
    assert_eq!(reply.tool_calls[0].id, "a");
    assert_eq!(reply.tool_calls[0].name, "add");
    assert_eq!(reply.tool_calls[0].arguments, json!({"a":2,"b":3}));
    assert_eq!(reply.tool_calls[1].arguments, json!({"q":"hi"}));
}

#[test]
fn rejected_finish_reasons_preserve_codes_and_usage_without_response_text() {
    for reason in ["length", "content_filter", "function_call", "unexpected"] {
        let mut acc = Accumulator::default();
        acc.push(text("private response")).unwrap();
        let mut value = finish(reason);
        value["usage"] = json!({"prompt_tokens":50,"completion_tokens":2048});
        let error = acc.push(value).unwrap_err().to_string();
        assert!(
            error.contains(&format!("finish_reason={reason}")),
            "{error}"
        );
        assert!(error.contains("completion_tokens=2048"), "{error}");
        assert!(!error.contains("private response"));
        assert!(acc.finish().is_err());
        let error = hive_agent_core::provider::parse_reply(json!({"choices":[{"finish_reason":reason,"message":{"content":"private response"}}],"usage":{"prompt_tokens":50,"completion_tokens":2048}})).unwrap_err().to_string();
        assert!(
            error.contains(&format!("finish_reason={reason}")),
            "{error}"
        );
        assert!(!error.contains("private response"));
    }
    let error = Accumulator::default()
        .push(finish("private response\nsecret"))
        .unwrap_err()
        .to_string();
    assert!(error.contains("finish_reason=INVALID"), "{error}");
    assert!(!error.contains("private response"));
}

#[test]
fn incomplete_or_truncated_streams_never_become_final_replies() {
    let mut acc = Accumulator::default();
    acc.push(text("partial")).unwrap();
    assert!(acc.finish().is_err());
    for reason in ["length", "content_filter", "function_call", "unexpected"] {
        assert!(Accumulator::default().push(finish(reason)).is_err());
    }
    let mut acc = Accumulator::default();
    acc.push(finish("tool_calls")).unwrap();
    assert!(acc.finish().is_err());
}

#[test]
fn tool_identity_changes_and_invalid_arguments_fail() {
    let mut acc = Accumulator::default();
    acc.push(json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"add","arguments":"{"}}]}}]})).unwrap();
    assert!(
        acc.push(json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"b"}]}}]}))
            .is_err()
    );
    acc.push(finish("tool_calls")).unwrap();
    assert!(acc.finish().is_err());
}

#[test]
fn missing_tool_index_or_duplicate_response_choice_is_not_a_complete_reply() {
    let mut acc = Accumulator::default();
    acc.push(
        json!({"choices":[{"delta":{"tool_calls":[{"index":1,"id":"b",
        "function":{"name":"add","arguments":"{}"}}]}}]}),
    )
    .unwrap();
    acc.push(finish("tool_calls")).unwrap();
    assert!(acc.finish().is_err());
    assert!(
        Accumulator::default()
            .push(json!({"choices":[{"index":0,"delta":{}},{"index":0,"delta":{}}]}))
            .is_err()
    );
}

#[test]
fn refusal_provider_errors_and_invalid_usage_fail() {
    assert!(
        Accumulator::default()
            .push(json!({"choices":[{"delta":{"refusal":"no"}}]}))
            .is_err()
    );
    assert!(
        Accumulator::default()
            .push(json!({"error":{"message":"private upstream detail"}}))
            .is_err()
    );
    assert!(
        Accumulator::default()
            .push(json!({"choices":[],"usage":{"prompt_tokens":-1}}))
            .is_err()
    );
    assert!(
        Accumulator::default()
            .push(json!({"choices":[{"index":1,"delta":{}}]}))
            .is_err()
    );
}

#[test]
fn content_after_finish_is_rejected_and_missing_usage_stays_unknown() {
    let mut acc = Accumulator::default();
    acc.push(text("hello")).unwrap();
    acc.push(finish("stop")).unwrap();
    assert!(acc.push(text("too late")).is_err());
    assert!(acc.finish().unwrap().usage.is_none());
}
