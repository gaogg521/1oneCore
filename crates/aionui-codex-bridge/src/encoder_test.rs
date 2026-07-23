use super::*;
use aion_types::message::{StopReason, TokenUsage};

fn names(events: &[SseEvent]) -> Vec<&'static str> {
    events.iter().map(|e| e.name).collect()
}

#[test]
fn text_only_response_emits_expected_sequence() {
    let mut enc = ResponsesEncoder::new("kimi-k3");
    let mut all = Vec::new();
    all.extend(enc.handle_event(LlmEvent::TextDelta("Hello".into())));
    all.extend(enc.handle_event(LlmEvent::TextDelta(", world".into())));
    all.extend(enc.handle_event(LlmEvent::Done {
        stop_reason: StopReason::EndTurn,
        usage: TokenUsage {
            input_tokens: 10,
            output_tokens: 5,
            ..Default::default()
        },
    }));

    assert_eq!(
        names(&all),
        vec![
            "response.output_item.added",
            "response.output_text.delta",
            "response.output_text.delta",
            "response.output_text.done",
            "response.output_item.done",
            "response.completed",
        ]
    );

    let completed = all.last().unwrap();
    assert_eq!(completed.data["response"]["status"], "completed");
    assert_eq!(completed.data["response"]["usage"]["input_tokens"], 10);
    assert_eq!(completed.data["response"]["usage"]["output_tokens"], 5);
    assert_eq!(
        completed.data["response"]["output"][0]["content"][0]["text"],
        "Hello, world"
    );
}

#[test]
fn tool_call_emits_full_added_delta_done_sequence() {
    let mut enc = ResponsesEncoder::new("kimi-k3");
    let events = enc.handle_event(LlmEvent::ToolUse {
        id: "call_abc".into(),
        name: "read_file".into(),
        input: serde_json::json!({"path": "a.py"}),
        extra: None,
    });

    assert_eq!(
        names(&events),
        vec![
            "response.output_item.added",
            "response.function_call_arguments.delta",
            "response.function_call_arguments.done",
            "response.output_item.done",
        ]
    );
    let done = events.last().unwrap();
    assert_eq!(done.data["item"]["call_id"], "call_abc");
    assert_eq!(done.data["item"]["name"], "read_file");
    let args: serde_json::Value = serde_json::from_str(done.data["item"]["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(args["path"], "a.py");
}

#[test]
fn thinking_then_text_closes_reasoning_item_before_opening_message() {
    let mut enc = ResponsesEncoder::new("kimi-k3");
    let mut all = Vec::new();
    all.extend(enc.handle_event(LlmEvent::ThinkingDelta("let me consider".into())));
    all.extend(enc.handle_event(LlmEvent::ThinkingSignature("sig-1".into())));
    all.extend(enc.handle_event(LlmEvent::TextDelta("Here is the answer".into())));
    all.extend(enc.handle_event(LlmEvent::Done {
        stop_reason: StopReason::EndTurn,
        usage: TokenUsage::default(),
    }));

    assert_eq!(
        names(&all),
        vec![
            "response.output_item.added", // reasoning item opens
            "response.output_item.done",  // reasoning item closes (no incremental delta event for reasoning summaries)
            "response.output_item.added", // message item opens
            "response.output_text.delta",
            "response.output_text.done",
            "response.output_item.done",
            "response.completed",
        ]
    );

    let reasoning_done = &all[1];
    let encrypted = reasoning_done.data["item"]["encrypted_content"].as_str().unwrap();
    let (thinking, signature) = crate::protocol::decode_thinking_token(encrypted).unwrap();
    assert_eq!(thinking, "let me consider");
    assert_eq!(signature.as_deref(), Some("sig-1"));
}

#[test]
fn truncated_tool_call_surfaces_as_error_and_marks_incomplete() {
    let mut enc = ResponsesEncoder::new("kimi-k3");
    let mut all = Vec::new();
    all.extend(enc.handle_event(LlmEvent::ToolCallTruncated {
        id: "call_x".into(),
        name: "write_file".into(),
    }));
    all.extend(enc.handle_event(LlmEvent::Done {
        stop_reason: StopReason::MaxTokens,
        usage: TokenUsage::default(),
    }));

    assert_eq!(names(&all), vec!["error", "response.completed"]);
    assert!(all[0].data["message"].as_str().unwrap().contains("write_file"));
    assert_eq!(all[1].data["response"]["status"], "incomplete");
}

#[test]
fn provider_error_marks_response_failed() {
    let mut enc = ResponsesEncoder::new("kimi-k3");
    let mut all = Vec::new();
    all.extend(enc.handle_event(LlmEvent::Error("gateway 500".into())));
    all.extend(enc.handle_event(LlmEvent::Done {
        stop_reason: StopReason::EndTurn,
        usage: TokenUsage::default(),
    }));

    assert_eq!(names(&all), vec!["error", "response.completed"]);
    assert_eq!(all[0].data["message"], "gateway 500");
    assert_eq!(all[1].data["response"]["status"], "failed");
}

#[test]
fn finalize_on_close_emits_terminal_events_when_channel_drops_early() {
    let mut enc = ResponsesEncoder::new("kimi-k3");
    enc.handle_event(LlmEvent::TextDelta("partial".into()));
    // finalize_on_close() must also close out the still-open text item
    // (`.done` + `output_item.done`) before appending the terminal
    // error/completed pair.
    let events = enc.finalize_on_close();

    assert_eq!(
        names(&events),
        vec![
            "response.output_text.done",
            "response.output_item.done",
            "error",
            "response.completed",
        ]
    );
    assert_eq!(events.last().unwrap().data["response"]["status"], "failed");
}
