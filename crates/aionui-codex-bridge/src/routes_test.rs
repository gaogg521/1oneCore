use std::sync::Arc;

use aionui_db::{SqliteCodexBridgeConfigRepository, SqliteProviderRepository, init_database_memory};
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use tower::ServiceExt;

use super::*;
use crate::service::CodexBridgeService;

async fn test_state() -> CodexBridgeRouterState {
    let db = init_database_memory().await.unwrap();
    let provider_repo = Arc::new(SqliteProviderRepository::new(db.pool().clone()));
    let config_repo = Arc::new(SqliteCodexBridgeConfigRepository::new(db.pool().clone()));
    // Leaked intentionally: the in-memory DB backing this test state must
    // outlive the router under test, and tests are short-lived processes.
    Box::leak(Box::new(db));
    CodexBridgeRouterState {
        service: Arc::new(CodexBridgeService::new(provider_repo, config_repo, [0u8; 32])),
    }
}

async fn body_json(response: axum::response::Response) -> serde_json::Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn responses_endpoint_rejects_missing_bearer_token() {
    let state = test_state().await;
    let app = codex_bridge_public_routes(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"model":"m","input":"hi"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn responses_endpoint_rejects_when_bridge_never_configured() {
    let state = test_state().await;
    let app = codex_bridge_public_routes(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::AUTHORIZATION, "Bearer whatever-token")
                .body(Body::from(r#"{"model":"m","input":"hi"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn responses_endpoint_rejects_wrong_bearer_token() {
    let state = test_state().await;
    state
        .service
        .upsert_config(true, Some("prov-1"), Some("kimi-k3"))
        .await
        .unwrap();
    let app = codex_bridge_public_routes(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/responses")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::AUTHORIZATION, "Bearer definitely-wrong")
                .body(Body::from(r#"{"model":"m","input":"hi"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn put_config_rejects_enabling_without_provider_and_model() {
    let state = test_state().await;
    let app = codex_bridge_config_routes(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/codex-bridge/config")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"enabled":true}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn get_config_reports_unconfigured_before_first_put() {
    let state = test_state().await;
    let app = codex_bridge_config_routes(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/codex-bridge/config")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response).await;
    assert_eq!(json["data"]["configured"], false);
    assert_eq!(json["data"]["enabled"], false);
}

#[tokio::test]
async fn put_then_get_config_round_trips() {
    let state = test_state().await;
    let app = codex_bridge_config_routes(state);

    let put_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/codex-bridge/config")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"enabled":true,"provider_id":"prov-1","model":"kimi-k3"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(put_response.status(), StatusCode::OK);

    let get_response = app
        .oneshot(
            Request::builder()
                .uri("/api/codex-bridge/config")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let json = body_json(get_response).await;
    assert_eq!(json["data"]["configured"], true);
    assert_eq!(json["data"]["enabled"], true);
    assert_eq!(json["data"]["provider_id"], "prov-1");
    assert_eq!(json["data"]["model"], "kimi-k3");
    // The bearer token must never be exposed over this API.
    assert!(json["data"].get("bearer_token").is_none());
}
