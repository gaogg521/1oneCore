use std::sync::Arc;

use aion_providers::create_provider;
use aionui_db::{CodexBridgeConfig, ICodexBridgeConfigRepository, IProviderRepository};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::encoder::{ResponsesEncoder, SseEvent};
use crate::error::BridgeError;
use crate::protocol::{ResponsesRequest, build_llm_request};

pub struct CodexBridgeService {
    provider_repo: Arc<dyn IProviderRepository>,
    config_repo: Arc<dyn ICodexBridgeConfigRepository>,
    encryption_key: [u8; 32],
}

impl CodexBridgeService {
    pub fn new(
        provider_repo: Arc<dyn IProviderRepository>,
        config_repo: Arc<dyn ICodexBridgeConfigRepository>,
        encryption_key: [u8; 32],
    ) -> Self {
        Self {
            provider_repo,
            config_repo,
            encryption_key,
        }
    }

    /// Returns the current bridge config, or `None` if never configured.
    pub async fn get_config(&self) -> Result<Option<CodexBridgeConfig>, BridgeError> {
        Ok(self.config_repo.get().await?)
    }

    /// Enable/disable the bridge and/or change which saved provider+model it
    /// forwards to. Generates a bearer token on first setup; subsequent calls
    /// preserve the existing token unless the caller explicitly wants a new
    /// one via [`Self::rotate_token`].
    pub async fn upsert_config(
        &self,
        enabled: bool,
        provider_id: Option<&str>,
        model: Option<&str>,
    ) -> Result<CodexBridgeConfig, BridgeError> {
        let token = match self.config_repo.get().await? {
            Some(existing) => existing.bearer_token,
            None => aionui_common::generate_id_with_length(Some(48)),
        };
        Ok(self.config_repo.upsert(enabled, provider_id, model, &token).await?)
    }

    /// Verify a caller-supplied bearer token against the stored config.
    /// Returns the config on success so callers don't need a second lookup.
    pub async fn authenticate(&self, bearer_token: &str) -> Result<CodexBridgeConfig, BridgeError> {
        let config = self.config_repo.get().await?.ok_or(BridgeError::NotConfigured)?;
        if !config.enabled {
            return Err(BridgeError::NotConfigured);
        }
        // Constant-time-ish check is unnecessary here: this is a
        // loopback-scoped local secret, not a network-facing credential
        // subject to remote timing attacks, and the token space (48 hex
        // chars, ~192 bits) makes online guessing infeasible regardless.
        if config.bearer_token != bearer_token {
            return Err(BridgeError::Unauthorized);
        }
        Ok(config)
    }

    /// Handle a `/v1/responses` request: resolve the configured provider,
    /// run the request through it, and produce either a single aggregated
    /// response or an SSE event stream depending on `request.stream`.
    pub async fn handle_responses_request(
        &self,
        config: &CodexBridgeConfig,
        request: ResponsesRequest,
    ) -> Result<ResponsesOutcome, BridgeError> {
        let provider_id = config
            .provider_id
            .as_deref()
            .ok_or_else(|| BridgeError::BadRequest("codex bridge has no provider configured".into()))?;
        let model = config
            .model
            .as_deref()
            .ok_or_else(|| BridgeError::BadRequest("codex bridge has no model configured".into()))?;

        let provider_config = aionui_ai_agent::resolve_provider_config_for_bridge(
            self.provider_repo.as_ref(),
            &self.encryption_key,
            provider_id,
            model,
        )
        .await?;
        let max_tokens = provider_config.compat.default_max_tokens_for_model(model);
        let llm_request = build_llm_request(&request, model, max_tokens.or(request.max_output_tokens));

        let provider = create_provider(&provider_config);
        let rx = provider.stream(&llm_request).await?;

        if request.stream {
            Ok(ResponsesOutcome::Stream(stream_events(rx, model.to_owned())))
        } else {
            Ok(ResponsesOutcome::Aggregated(
                aggregate_response(rx, model.to_owned()).await,
            ))
        }
    }
}

pub enum ResponsesOutcome {
    Aggregated(Value),
    Stream(mpsc::Receiver<SseEvent>),
}

/// Drain the whole provider event stream and build one non-streaming
/// Responses API response body.
async fn aggregate_response(mut rx: mpsc::Receiver<aion_types::llm::LlmEvent>, model: String) -> Value {
    let mut encoder = ResponsesEncoder::new(&model);
    let mut last_completed: Option<Value> = None;

    while let Some(event) = rx.recv().await {
        let is_done = matches!(event, aion_types::llm::LlmEvent::Done { .. });
        for sse in encoder.handle_event(event) {
            if sse.name == "response.completed" {
                last_completed = sse.data.get("response").cloned();
            }
        }
        if is_done {
            break;
        }
    }

    last_completed.unwrap_or_else(|| {
        json!({
            "id": format!("resp_{}", aionui_common::generate_id()),
            "object": "response",
            "model": model,
            "status": "failed",
            "output": [],
            "error": { "message": "provider stream closed without completing" },
        })
    })
}

/// Bridge the provider's event channel into an SSE event channel, encoding
/// each `LlmEvent` as it arrives (not buffered) so long-running generations
/// (extended thinking, large tool outputs) don't sit silently until the
/// whole turn finishes before Codex sees any bytes.
fn stream_events(mut rx: mpsc::Receiver<aion_types::llm::LlmEvent>, model: String) -> mpsc::Receiver<SseEvent> {
    let (tx, out_rx) = mpsc::channel(64);

    tokio::spawn(async move {
        let mut encoder = ResponsesEncoder::new(&model);
        loop {
            match rx.recv().await {
                Some(event) => {
                    let is_done = matches!(event, aion_types::llm::LlmEvent::Done { .. });
                    for sse in encoder.handle_event(event) {
                        if tx.send(sse).await.is_err() {
                            return; // client disconnected
                        }
                    }
                    if is_done {
                        return;
                    }
                }
                None => {
                    for sse in encoder.finalize_on_close() {
                        if tx.send(sse).await.is_err() {
                            return;
                        }
                    }
                    return;
                }
            }
        }
    });

    out_rx
}

#[cfg(test)]
#[path = "service_test.rs"]
mod service_test;
