//! Minimal subset of the OpenAI Responses API wire format (the only
//! `wire_api` Codex CLI currently supports) that this bridge actually needs:
//! request parsing into the provider-neutral [`aion_types::llm::LlmRequest`],
//! and the reverse mapping used by the streaming/aggregating encoders.
//!
//! Unknown fields are ignored rather than rejected — Codex may send
//! additional Responses API fields (`store`, `parallel_tool_calls`, ...)
//! this bridge does not act on.

use aion_types::llm::LlmRequest;
use aion_types::message::{ContentBlock, ImageUrl, Message, Role};
use aion_types::tool::ToolDef;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::Deserialize;
use serde_json::Value;

/// Self-describing (not actually encrypted — there is no need for real
/// encryption here, only a round-trippable opaque token) marker for the
/// thinking blob this bridge hands back to Codex as a reasoning item's
/// `encrypted_content`. Codex must not interpret it; it just replays the
/// token verbatim on the next turn, at which point we decode it back into
/// the real thinking text + signature the underlying provider needs.
const THINKING_TOKEN_PREFIX: &str = "onework-thinking-v1:";

pub fn encode_thinking_token(thinking: &str, signature: Option<&str>) -> String {
    let payload = serde_json::json!({ "thinking": thinking, "signature": signature });
    format!("{THINKING_TOKEN_PREFIX}{}", BASE64.encode(payload.to_string()))
}

pub(crate) fn decode_thinking_token(token: &str) -> Option<(String, Option<String>)> {
    let b64 = token.strip_prefix(THINKING_TOKEN_PREFIX)?;
    let bytes = BASE64.decode(b64).ok()?;
    let json: Value = serde_json::from_slice(&bytes).ok()?;
    let thinking = json.get("thinking")?.as_str()?.to_owned();
    let signature = json.get("signature").and_then(|v| v.as_str()).map(str::to_owned);
    Some((thinking, signature))
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResponsesRequest {
    pub model: String,
    #[serde(default)]
    pub instructions: Option<String>,
    #[serde(default)]
    pub input: ResponsesInput,
    #[serde(default)]
    pub tools: Vec<ResponsesTool>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    #[serde(default)]
    pub reasoning: Option<ReasoningConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ReasoningConfig {
    #[serde(default)]
    pub effort: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(untagged)]
pub enum ResponsesInput {
    #[default]
    Empty,
    Text(String),
    Items(Vec<InputItem>),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputItem {
    Message {
        role: String,
        content: Vec<InputContent>,
    },
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    FunctionCallOutput {
        call_id: String,
        output: Value,
    },
    Reasoning {
        #[serde(default)]
        encrypted_content: Option<String>,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum InputContent {
    #[serde(rename = "input_text")]
    InputText { text: String },
    #[serde(rename = "output_text")]
    OutputText { text: String },
    #[serde(rename = "input_image")]
    InputImage {
        #[serde(default)]
        image_url: Option<String>,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResponsesTool {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub parameters: Value,
}

fn map_role(role: &str) -> Role {
    match role {
        "assistant" => Role::Assistant,
        "system" | "developer" => Role::System,
        "tool" => Role::Tool,
        _ => Role::User,
    }
}

fn function_call_output_text(output: &Value) -> String {
    match output {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Build a provider-neutral [`LlmRequest`] from a parsed Responses API
/// request. `model` is passed separately since the bridge resolves it from
/// saved provider config, not from the (possibly Codex-side-only) model name
/// in the request body.
pub fn build_llm_request(req: &ResponsesRequest, model: &str, max_tokens: Option<u32>) -> LlmRequest {
    let mut messages = Vec::new();

    let items: &[InputItem] = match &req.input {
        ResponsesInput::Empty => &[],
        ResponsesInput::Text(text) => {
            messages.push(Message::new(
                Role::User,
                vec![ContentBlock::Text { text: text.clone() }],
            ));
            &[]
        }
        ResponsesInput::Items(items) => items,
    };

    for item in items {
        match item {
            InputItem::Message { role, content } => {
                let blocks: Vec<ContentBlock> = content
                    .iter()
                    .filter_map(|c| match c {
                        InputContent::InputText { text } | InputContent::OutputText { text } => {
                            Some(ContentBlock::Text { text: text.clone() })
                        }
                        InputContent::InputImage { image_url } => image_url.clone().map(|url| ContentBlock::Image {
                            image_url: ImageUrl { url },
                        }),
                        InputContent::Unknown => None,
                    })
                    .collect();
                if !blocks.is_empty() {
                    messages.push(Message::new(map_role(role), blocks));
                }
            }
            InputItem::FunctionCall {
                call_id,
                name,
                arguments,
            } => {
                let input = serde_json::from_str(arguments).unwrap_or(Value::Null);
                messages.push(Message::new(
                    Role::Assistant,
                    vec![ContentBlock::ToolUse {
                        id: call_id.clone(),
                        name: name.clone(),
                        input,
                        extra: None,
                    }],
                ));
            }
            InputItem::FunctionCallOutput { call_id, output } => {
                messages.push(Message::new(
                    Role::Tool,
                    vec![ContentBlock::ToolResult {
                        tool_use_id: call_id.clone(),
                        content: function_call_output_text(output),
                        is_error: false,
                    }],
                ));
            }
            InputItem::Reasoning { encrypted_content } => {
                if let Some((thinking, signature)) = encrypted_content.as_deref().and_then(decode_thinking_token) {
                    messages.push(Message::new(
                        Role::Assistant,
                        vec![ContentBlock::Thinking { thinking, signature }],
                    ));
                }
            }
            InputItem::Unknown => {}
        }
    }

    let tools = req
        .tools
        .iter()
        .filter(|t| t.kind == "function")
        .filter_map(|t| {
            t.name.clone().map(|name| ToolDef {
                name,
                description: t.description.clone().unwrap_or_default(),
                input_schema: t.parameters.clone(),
                deferred: false,
            })
        })
        .collect();

    LlmRequest {
        model: model.to_owned(),
        system: req.instructions.clone().unwrap_or_default(),
        messages,
        tools,
        max_tokens,
        thinking: None,
        reasoning_effort: req.reasoning.as_ref().and_then(|r| r.effort.clone()),
    }
}

#[cfg(test)]
#[path = "protocol_test.rs"]
mod protocol_test;
