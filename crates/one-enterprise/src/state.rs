//! Router state for one-enterprise routes.

use std::sync::Arc;

use crate::service::EnterpriseService;

#[derive(Clone)]
pub struct OneEnterpriseRouterState {
    pub service: Arc<EnterpriseService>,
}

impl OneEnterpriseRouterState {
    pub fn new(service: Arc<EnterpriseService>) -> Self {
        Self { service }
    }
}
