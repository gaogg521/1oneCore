//! Router state carrying the assistant service for axum handlers.

use std::sync::Arc;

use aionui_db::IAssistantMarketplaceRepository;

use crate::service::AssistantService;

/// Shared state injected into `/api/assistants/*` handlers.
#[derive(Clone)]
pub struct AssistantRouterState {
    pub service: Arc<AssistantService>,
    /// Separate from `service` on purpose — the marketplace catalog never
    /// touches `assistant_definitions`, so it doesn't belong inside
    /// `AssistantService`. See `crates/aionui-assistant/src/marketplace.rs`.
    pub marketplace_repo: Arc<dyn IAssistantMarketplaceRepository>,
}
