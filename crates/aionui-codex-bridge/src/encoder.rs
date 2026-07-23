//! Maps provider-neutral [`LlmEvent`]s (from `aion_providers::LlmProvider::stream`)
//! into OpenAI Responses API SSE events / a single aggregated response body.
//!
//! `aion-providers` already accumulates streaming tool-call deltas into one
//! complete [`LlmEvent::ToolUse`] per call (see its doc comment), so this
//! encoder never has to reassemble partial tool-call JSON itself — it only
//! has to re-emit that one complete call as the `output_item.added` /
//! `function_call_arguments.delta` / `.done` / `output_item.done` sequence
//! Responses API clients expect.
//!
//! Produces plain `(event name, JSON payload)` pairs rather than
//! `axum::response::sse::Event` directly, so encoder logic is testable
//! without depending on axum's SSE wire framing.

use aion_types::llm::LlmEvent;
use aion_types::message::StopReason;
use serde_json::{Value, json};

use crate::protocol::encode_thinking_token;

/// One SSE frame: `event: {name}\ndata: {data}\n\n`.
#[derive(Debug, Clone, PartialEq)]
pub struct SseEvent {
    pub name: &'static str,
    pub data: Value,
}

enum OpenItem {
    Text {
        item_id: String,
        text: String,
    },
    Reasoning {
        item_id: String,
        thinking: String,
        signature: Option<String>,
    },
}

pub struct ResponsesEncoder {
    response_id: String,
    model: String,
    output_index: u32,
    sequence_number: u64,
    current: Option<OpenItem>,
    completed_output: Vec<Value>,
    truncated: bool,
    error: Option<String>,
}

impl ResponsesEncoder {
    pub fn new(model: &str) -> Self {
        Self {
            response_id: format!("resp_{}", aionui_common::generate_id()),
            model: model.to_owned(),
            output_index: 0,
            sequence_number: 0,
            current: None,
            completed_output: Vec::new(),
            truncated: false,
            error: None,
        }
    }

    fn next_seq(&mut self) -> u64 {
        self.sequence_number += 1;
        self.sequence_number
    }

    fn response_stub(&self, status: &str) -> Value {
        json!({
            "id": self.response_id,
            "object": "response",
            "model": self.model,
            "status": status,
        })
    }

    /// Close whatever output item is currently open, emitting its `.done`
    /// events and recording the completed item for the final response body.
    fn close_current(&mut self, out: &mut Vec<SseEvent>) {
        let Some(item) = self.current.take() else { return };
        let index = self.output_index;
        self.output_index += 1;

        match item {
            OpenItem::Text { item_id, text } => {
                out.push(SseEvent {
                    name: "response.output_text.done",
                    data: json!({
                        "type": "response.output_text.done",
                        "sequence_number": self.next_seq(),
                        "item_id": item_id,
                        "output_index": index,
                        "content_index": 0,
                        "text": text,
                    }),
                });
                let completed = json!({
                    "id": item_id,
                    "type": "message",
                    "status": "completed",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": text, "annotations": []}],
                });
                out.push(SseEvent {
                    name: "response.output_item.done",
                    data: json!({
                        "type": "response.output_item.done",
                        "sequence_number": self.next_seq(),
                        "output_index": index,
                        "item": completed,
                    }),
                });
                self.completed_output.push(completed);
            }
            OpenItem::Reasoning {
                item_id,
                thinking,
                signature,
            } => {
                let encrypted_content = encode_thinking_token(&thinking, signature.as_deref());
                let completed = json!({
                    "id": item_id,
                    "type": "reasoning",
                    "summary": [],
                    "encrypted_content": encrypted_content,
                });
                out.push(SseEvent {
                    name: "response.output_item.done",
                    data: json!({
                        "type": "response.output_item.done",
                        "sequence_number": self.next_seq(),
                        "output_index": index,
                        "item": completed,
                    }),
                });
                self.completed_output.push(completed);
            }
        }
    }

    /// Handle one provider event, returning zero or more SSE events to emit
    /// (in order). Returns an empty vec for events that only update internal
    /// state (e.g. a thinking signature that arrives before the item closes).
    pub fn handle_event(&mut self, event: LlmEvent) -> Vec<SseEvent> {
        let mut out = Vec::new();

        match event {
            LlmEvent::TextDelta(delta) => {
                if !matches!(self.current, Some(OpenItem::Text { .. })) {
                    self.close_current(&mut out);
                    let item_id = format!("msg_{}", aionui_common::generate_id());
                    out.push(SseEvent {
                        name: "response.output_item.added",
                        data: json!({
                            "type": "response.output_item.added",
                            "sequence_number": self.next_seq(),
                            "output_index": self.output_index,
                            "item": {
                                "id": item_id,
                                "type": "message",
                                "status": "in_progress",
                                "role": "assistant",
                                "content": [],
                            },
                        }),
                    });
                    self.current = Some(OpenItem::Text {
                        item_id,
                        text: String::new(),
                    });
                }
                if let Some(OpenItem::Text { item_id, text }) = &mut self.current {
                    text.push_str(&delta);
                    let item_id = item_id.clone();
                    let output_index = self.output_index;
                    let seq = self.next_seq();
                    out.push(SseEvent {
                        name: "response.output_text.delta",
                        data: json!({
                            "type": "response.output_text.delta",
                            "sequence_number": seq,
                            "item_id": item_id,
                            "output_index": output_index,
                            "content_index": 0,
                            "delta": delta,
                        }),
                    });
                }
            }
            LlmEvent::ThinkingDelta(delta) => {
                if !matches!(self.current, Some(OpenItem::Reasoning { .. })) {
                    self.close_current(&mut out);
                    let item_id = format!("rs_{}", aionui_common::generate_id());
                    out.push(SseEvent {
                        name: "response.output_item.added",
                        data: json!({
                            "type": "response.output_item.added",
                            "sequence_number": self.next_seq(),
                            "output_index": self.output_index,
                            "item": {
                                "id": item_id,
                                "type": "reasoning",
                                "summary": [],
                            },
                        }),
                    });
                    self.current = Some(OpenItem::Reasoning {
                        item_id,
                        thinking: String::new(),
                        signature: None,
                    });
                }
                if let Some(OpenItem::Reasoning { thinking, .. }) = &mut self.current {
                    thinking.push_str(&delta);
                }
            }
            LlmEvent::ThinkingSignature(sig) => {
                if let Some(OpenItem::Reasoning { signature, .. }) = &mut self.current {
                    *signature = Some(sig);
                }
            }
            LlmEvent::ToolUse { id, name, input, .. } => {
                self.close_current(&mut out);
                let index = self.output_index;
                self.output_index += 1;
                let arguments = input.to_string();

                out.push(SseEvent {
                    name: "response.output_item.added",
                    data: json!({
                        "type": "response.output_item.added",
                        "sequence_number": self.next_seq(),
                        "output_index": index,
                        "item": {
                            "id": format!("fc_{id}"),
                            "type": "function_call",
                            "status": "in_progress",
                            "call_id": id,
                            "name": name,
                            "arguments": "",
                        },
                    }),
                });
                out.push(SseEvent {
                    name: "response.function_call_arguments.delta",
                    data: json!({
                        "type": "response.function_call_arguments.delta",
                        "sequence_number": self.next_seq(),
                        "item_id": format!("fc_{id}"),
                        "output_index": index,
                        "delta": arguments,
                    }),
                });
                out.push(SseEvent {
                    name: "response.function_call_arguments.done",
                    data: json!({
                        "type": "response.function_call_arguments.done",
                        "sequence_number": self.next_seq(),
                        "item_id": format!("fc_{id}"),
                        "output_index": index,
                        "arguments": arguments,
                    }),
                });
                let completed = json!({
                    "id": format!("fc_{id}"),
                    "type": "function_call",
                    "status": "completed",
                    "call_id": id,
                    "name": name,
                    "arguments": arguments,
                });
                out.push(SseEvent {
                    name: "response.output_item.done",
                    data: json!({
                        "type": "response.output_item.done",
                        "sequence_number": self.next_seq(),
                        "output_index": index,
                        "item": completed,
                    }),
                });
                self.completed_output.push(completed);
            }
            LlmEvent::ToolCallTruncated { id, name } => {
                self.truncated = true;
                tracing::warn!(
                    tool_call_id = %id,
                    tool_name = %name,
                    "codex-bridge: tool call truncated by provider output limit"
                );
                out.push(SseEvent {
                    name: "error",
                    data: json!({
                        "type": "error",
                        "sequence_number": self.next_seq(),
                        "message": format!(
                            "Tool call '{name}' was truncated by the model's output limit and was not executed. Retry the request."
                        ),
                    }),
                });
            }
            LlmEvent::ProviderItem { .. } => {
                // Opaque items owned by a different provider transport than
                // the one this bridge forwards to; nothing to surface.
            }
            LlmEvent::Error(message) => {
                self.close_current(&mut out);
                self.error = Some(message.clone());
                out.push(SseEvent {
                    name: "error",
                    data: json!({
                        "type": "error",
                        "sequence_number": self.next_seq(),
                        "message": message,
                    }),
                });
            }
            LlmEvent::Done { stop_reason, usage } => {
                self.close_current(&mut out);
                let status = if self.error.is_some() {
                    "failed"
                } else if self.truncated || matches!(stop_reason, StopReason::MaxTokens) {
                    "incomplete"
                } else {
                    "completed"
                };
                let mut response = self.response_stub(status);
                response["output"] = Value::Array(self.completed_output.clone());
                response["usage"] = json!({
                    "input_tokens": usage.input_tokens,
                    "output_tokens": usage.output_tokens,
                    "total_tokens": usage.input_tokens + usage.output_tokens,
                });
                out.push(SseEvent {
                    name: "response.completed",
                    data: json!({
                        "type": "response.completed",
                        "sequence_number": self.next_seq(),
                        "response": response,
                    }),
                });
            }
        }

        out
    }

    /// The provider's event channel closed without ever emitting `Done`
    /// (e.g. the underlying task was dropped). Finalize defensively so the
    /// SSE stream still ends with a terminal event instead of just hanging
    /// up, which would leave Codex waiting indefinitely.
    pub fn finalize_on_close(&mut self) -> Vec<SseEvent> {
        let mut out = Vec::new();
        self.close_current(&mut out);
        let mut response = self.response_stub("failed");
        response["output"] = Value::Array(self.completed_output.clone());
        out.push(SseEvent {
            name: "error",
            data: json!({
                "type": "error",
                "sequence_number": self.next_seq(),
                "message": "upstream provider connection closed before the response completed",
            }),
        });
        out.push(SseEvent {
            name: "response.completed",
            data: json!({
                "type": "response.completed",
                "sequence_number": self.next_seq(),
                "response": response,
            }),
        });
        out
    }
}

#[cfg(test)]
#[path = "encoder_test.rs"]
mod encoder_test;
