use std::sync::Arc;

use crate::service::CodexBridgeService;

#[derive(Clone)]
pub struct CodexBridgeRouterState {
    pub service: Arc<CodexBridgeService>,
}
