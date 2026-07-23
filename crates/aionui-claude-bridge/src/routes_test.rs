use std::sync::Arc;

use aionui_db::{SqliteClaudeBridgeConfigRepository, init_database_memory};
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use tower::ServiceExt;

use super::*;
use crate::service::ClaudeBridgeService;

async fn test_state() -> ClaudeBridgeRouterState {
    let db = init_database_memory().await.unwrap();
    let config_repo = Arc::new(SqliteClaudeBridgeConfigRepository::new(db.pool().clone()));
    // Leaked intentionally: the in-memory DB backing this test state must
    // outlive the router under test, and tests are short-lived processes.
    Box::leak(Box::new(db));
    ClaudeBridgeRouterState {
        service: Arc::new(ClaudeBridgeService::new(config_repo)),
    }
}

async fn body_json(response: axum::response::Response) -> serde_json::Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn put_config_rejects_enabling_without_provider_and_model() {
    let state = test_state().await;
    let app = claude_bridge_config_routes(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/claude-bridge/config")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"enabled":true}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn get_config_reports_disabled_before_first_put() {
    let state = test_state().await;
    let app = claude_bridge_config_routes(state);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/claude-bridge/config")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response).await;
    assert_eq!(json["data"]["enabled"], false);
    assert!(json["data"]["provider_id"].is_null());
}

#[tokio::test]
async fn put_then_get_config_round_trips() {
    let state = test_state().await;
    let app = claude_bridge_config_routes(state);

    let put_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/claude-bridge/config")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"enabled":true,"provider_id":"prov-1","model":"glm-5-2"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(put_response.status(), StatusCode::OK);

    let get_response = app
        .oneshot(
            Request::builder()
                .uri("/api/claude-bridge/config")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let json = body_json(get_response).await;
    assert_eq!(json["data"]["enabled"], true);
    assert_eq!(json["data"]["provider_id"], "prov-1");
    assert_eq!(json["data"]["model"], "glm-5-2");
}

#[tokio::test]
async fn put_config_allows_disabling_without_provider_and_model() {
    let state = test_state().await;
    let app = claude_bridge_config_routes(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/claude-bridge/config")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"enabled":false}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response).await;
    assert_eq!(json["data"]["enabled"], false);
}
