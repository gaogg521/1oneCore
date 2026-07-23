use super::*;
use aion_types::message::Role;

fn parse(json: &str) -> ResponsesRequest {
    serde_json::from_str(json).expect("valid ResponsesRequest JSON")
}

#[test]
fn thinking_token_round_trips() {
    let token = encode_thinking_token("let me think about this", Some("sig-123"));
    assert!(token.starts_with(THINKING_TOKEN_PREFIX));

    let (thinking, signature) = decode_thinking_token(&token).expect("token decodes");
    assert_eq!(thinking, "let me think about this");
    assert_eq!(signature.as_deref(), Some("sig-123"));
}

#[test]
fn decode_thinking_token_rejects_foreign_tokens() {
    assert!(decode_thinking_token("not-our-token").is_none());
    assert!(decode_thinking_token("onework-thinking-v1:not-base64!!!").is_none());
}

#[test]
fn plain_string_input_becomes_single_user_message() {
    let req = parse(r#"{"model":"placeholder","input":"say hi"}"#);
    let llm = build_llm_request(&req, "kimi-k3", None);

    assert_eq!(llm.messages.len(), 1);
    assert_eq!(llm.messages[0].role, Role::User);
    match &llm.messages[0].content[0] {
        ContentBlock::Text { text } => assert_eq!(text, "say hi"),
        other => panic!("expected text block, got {other:?}"),
    }
}

#[test]
fn multi_turn_history_with_tool_calls_and_reasoning_maps_correctly() {
    let token = encode_thinking_token("reasoning about the fix", Some("sig-abc"));
    let json = format!(
        r#"{{
            "model": "placeholder",
            "instructions": "You are a coding agent.",
            "input": [
                {{"type":"message","role":"user","content":[{{"type":"input_text","text":"fix the bug"}}]}},
                {{"type":"reasoning","encrypted_content":"{token}"}},
                {{"type":"function_call","call_id":"call_1","name":"read_file","arguments":"{{\"path\":\"a.py\"}}"}},
                {{"type":"function_call_output","call_id":"call_1","output":"file contents here"}}
            ],
            "tools": [
                {{"type":"function","name":"read_file","description":"Read a file","parameters":{{"type":"object"}}}}
            ]
        }}"#
    );
    let req = parse(&json);
    let llm = build_llm_request(&req, "kimi-k3", Some(4096));

    assert_eq!(llm.system, "You are a coding agent.");
    assert_eq!(llm.max_tokens, Some(4096));
    assert_eq!(llm.tools.len(), 1);
    assert_eq!(llm.tools[0].name, "read_file");

    assert_eq!(llm.messages.len(), 4);
    assert_eq!(llm.messages[0].role, Role::User);

    assert_eq!(llm.messages[1].role, Role::Assistant);
    match &llm.messages[1].content[0] {
        ContentBlock::Thinking { thinking, signature } => {
            assert_eq!(thinking, "reasoning about the fix");
            assert_eq!(signature.as_deref(), Some("sig-abc"));
        }
        other => panic!("expected thinking block, got {other:?}"),
    }

    assert_eq!(llm.messages[2].role, Role::Assistant);
    match &llm.messages[2].content[0] {
        ContentBlock::ToolUse { id, name, input, .. } => {
            assert_eq!(id, "call_1");
            assert_eq!(name, "read_file");
            assert_eq!(input["path"], "a.py");
        }
        other => panic!("expected tool_use block, got {other:?}"),
    }

    assert_eq!(llm.messages[3].role, Role::Tool);
    match &llm.messages[3].content[0] {
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => {
            assert_eq!(tool_use_id, "call_1");
            assert_eq!(content, "file contents here");
            assert!(!is_error);
        }
        other => panic!("expected tool_result block, got {other:?}"),
    }
}

#[test]
fn reasoning_effort_is_forwarded() {
    let req = parse(r#"{"model":"placeholder","input":"hi","reasoning":{"effort":"high"}}"#);
    let llm = build_llm_request(&req, "kimi-k3", None);
    assert_eq!(llm.reasoning_effort.as_deref(), Some("high"));
}

#[test]
fn non_function_tools_are_ignored() {
    let req = parse(
        r#"{"model":"placeholder","input":"hi","tools":[{"type":"web_search"},{"type":"function","name":"noop","parameters":{}}]}"#,
    );
    let llm = build_llm_request(&req, "kimi-k3", None);
    assert_eq!(llm.tools.len(), 1);
    assert_eq!(llm.tools[0].name, "noop");
}

#[test]
fn unknown_input_item_types_are_skipped_without_error() {
    let req = parse(
        r#"{"model":"placeholder","input":[{"type":"some_future_item_type","foo":"bar"},{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#,
    );
    let llm = build_llm_request(&req, "kimi-k3", None);
    assert_eq!(llm.messages.len(), 1);
}
