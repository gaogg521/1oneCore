use std::sync::Arc;

use crate::service::ClaudeBridgeService;

#[derive(Clone)]
pub struct ClaudeBridgeRouterState {
    pub service: Arc<ClaudeBridgeService>,
}
