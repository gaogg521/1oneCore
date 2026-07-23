#![allow(clippy::disallowed_types)]

use aionui_api_types::ApiResponse;
use aionui_common::ApiError;
use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::routing::get;
use serde::{Deserialize, Serialize};

use crate::state::ClaudeBridgeRouterState;

/// `GET`/`PUT /api/claude-bridge/config` — app-facing settings for the
/// Claude Code custom-provider bridge, gated by the caller's normal session
/// `auth_middleware` at registration time (see `aionui-app`'s router
/// wiring), same as every other authenticated route.
pub fn claude_bridge_config_routes(state: ClaudeBridgeRouterState) -> Router {
    Router::new()
        .route("/api/claude-bridge/config", get(get_config).put(put_config))
        .with_state(state)
}

#[derive(Debug, Serialize)]
struct ClaudeBridgeConfigDto {
    enabled: bool,
    provider_id: Option<String>,
    model: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UpdateClaudeBridgeConfigRequest {
    enabled: bool,
    provider_id: Option<String>,
    model: Option<String>,
}

async fn get_config(
    State(state): State<ClaudeBridgeRouterState>,
) -> Result<Json<ApiResponse<ClaudeBridgeConfigDto>>, ApiError> {
    let config = state
        .service
        .get_config()
        .await
        .map_err(|err| ApiError::Internal(err.to_string()))?;

    let dto = match config {
        Some(c) => ClaudeBridgeConfigDto {
            enabled: c.enabled,
            provider_id: c.provider_id,
            model: c.model,
        },
        None => ClaudeBridgeConfigDto {
            enabled: false,
            provider_id: None,
            model: None,
        },
    };

    Ok(Json(ApiResponse::ok(dto)))
}

async fn put_config(
    State(state): State<ClaudeBridgeRouterState>,
    Json(body): Json<UpdateClaudeBridgeConfigRequest>,
) -> Result<Json<ApiResponse<ClaudeBridgeConfigDto>>, ApiError> {
    if body.enabled && (body.provider_id.is_none() || body.model.is_none()) {
        return Err(ApiError::BadRequest(
            "provider_id and model are required to enable the claude bridge".into(),
        ));
    }

    let config = state
        .service
        .upsert_config(body.enabled, body.provider_id.as_deref(), body.model.as_deref())
        .await
        .map_err(|err| ApiError::Internal(err.to_string()))?;

    Ok(Json(ApiResponse::ok(ClaudeBridgeConfigDto {
        enabled: config.enabled,
        provider_id: config.provider_id,
        model: config.model,
    })))
}

#[cfg(test)]
#[path = "routes_test.rs"]
mod routes_test;
