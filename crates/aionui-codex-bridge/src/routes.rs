#![allow(clippy::disallowed_types)]

use std::convert::Infallible;

use aionui_api_types::ApiResponse;
use aionui_common::ApiError;
use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures_util::Stream;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error::BridgeError;
use crate::protocol::ResponsesRequest;
use crate::service::ResponsesOutcome;
use crate::state::CodexBridgeRouterState;

/// Routes making up the Codex compatibility bridge:
/// - `POST /v1/responses` — the OpenAI Responses API surface Codex CLI talks
///   to. Gated by its own bearer token (not the app's session auth — Codex
///   is an external process with no browser session).
/// - `GET`/`PUT /api/codex-bridge/config` — app-facing settings, gated by the
///   caller's normal session `auth_middleware` at registration time (see
///   `aionui-app`'s router wiring), same as every other authenticated route.
pub fn codex_bridge_public_routes(state: CodexBridgeRouterState) -> Router {
    Router::new()
        .route("/v1/responses", post(handle_responses))
        .with_state(state)
}

pub fn codex_bridge_config_routes(state: CodexBridgeRouterState) -> Router {
    Router::new()
        .route("/api/codex-bridge/config", get(get_config).put(put_config))
        .with_state(state)
}

fn openai_error(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        Json(json!({ "error": { "message": message.into(), "type": "invalid_request_error" } })),
    )
        .into_response()
}

async fn handle_responses(
    State(state): State<CodexBridgeRouterState>,
    headers: HeaderMap,
    Json(request): Json<ResponsesRequest>,
) -> Response {
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));

    let Some(token) = token else {
        return openai_error(StatusCode::UNAUTHORIZED, "missing bearer token");
    };

    let config = match state.service.authenticate(token).await {
        Ok(config) => config,
        Err(BridgeError::NotConfigured) => {
            return openai_error(StatusCode::SERVICE_UNAVAILABLE, "codex bridge is not enabled");
        }
        Err(BridgeError::Unauthorized) => {
            return openai_error(StatusCode::UNAUTHORIZED, "invalid bearer token");
        }
        Err(err) => {
            tracing::error!(error = %err, "codex-bridge: config lookup failed");
            return openai_error(StatusCode::INTERNAL_SERVER_ERROR, "internal error");
        }
    };

    let stream_requested = request.stream;
    match state.service.handle_responses_request(&config, request).await {
        Ok(ResponsesOutcome::Aggregated(body)) => Json(body).into_response(),
        Ok(ResponsesOutcome::Stream(rx)) => Sse::new(receiver_stream(rx)).into_response(),
        Err(BridgeError::BadRequest(message)) => openai_error(StatusCode::BAD_REQUEST, message),
        Err(err) if stream_requested => {
            tracing::error!(error = %err, "codex-bridge: request failed before streaming started");
            openai_error(StatusCode::BAD_GATEWAY, err.to_string())
        }
        Err(err) => {
            tracing::error!(error = %err, "codex-bridge: request failed");
            openai_error(StatusCode::BAD_GATEWAY, err.to_string())
        }
    }
}

fn receiver_stream(
    mut rx: tokio::sync::mpsc::Receiver<crate::encoder::SseEvent>,
) -> impl Stream<Item = Result<Event, Infallible>> {
    futures_util::stream::poll_fn(move |cx| {
        rx.poll_recv(cx)
            .map(|opt| opt.map(|sse| Ok(Event::default().event(sse.name).data(sse.data.to_string()))))
    })
}

#[derive(Debug, Serialize)]
struct CodexBridgeConfigDto {
    enabled: bool,
    provider_id: Option<String>,
    model: Option<String>,
    /// Whether a bearer token has been generated. The token value itself is
    /// never exposed over this API — the bridge and the Codex launch-policy
    /// code both read it directly from the database.
    configured: bool,
}

#[derive(Debug, Deserialize)]
struct UpdateCodexBridgeConfigRequest {
    enabled: bool,
    provider_id: Option<String>,
    model: Option<String>,
}

async fn get_config(
    State(state): State<CodexBridgeRouterState>,
) -> Result<Json<ApiResponse<CodexBridgeConfigDto>>, ApiError> {
    let config = state
        .service
        .get_config()
        .await
        .map_err(|err| ApiError::Internal(err.to_string()))?;

    let dto = match config {
        Some(c) => CodexBridgeConfigDto {
            enabled: c.enabled,
            provider_id: c.provider_id,
            model: c.model,
            configured: true,
        },
        None => CodexBridgeConfigDto {
            enabled: false,
            provider_id: None,
            model: None,
            configured: false,
        },
    };

    Ok(Json(ApiResponse::ok(dto)))
}

async fn put_config(
    State(state): State<CodexBridgeRouterState>,
    Json(body): Json<UpdateCodexBridgeConfigRequest>,
) -> Result<Json<ApiResponse<CodexBridgeConfigDto>>, ApiError> {
    if body.enabled && (body.provider_id.is_none() || body.model.is_none()) {
        return Err(ApiError::BadRequest(
            "provider_id and model are required to enable the codex bridge".into(),
        ));
    }

    let config = state
        .service
        .upsert_config(body.enabled, body.provider_id.as_deref(), body.model.as_deref())
        .await
        .map_err(|err| ApiError::Internal(err.to_string()))?;

    Ok(Json(ApiResponse::ok(CodexBridgeConfigDto {
        enabled: config.enabled,
        provider_id: config.provider_id,
        model: config.model,
        configured: true,
    })))
}

#[cfg(test)]
#[path = "routes_test.rs"]
mod routes_test;
