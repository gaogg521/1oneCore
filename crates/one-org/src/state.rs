//! Router state for one-org routes.

use std::sync::Arc;

use crate::service::OrgService;

#[derive(Clone)]
pub struct OneOrgRouterState {
    pub service: Arc<OrgService>,
}

impl OneOrgRouterState {
    pub fn new(service: Arc<OrgService>) -> Self {
        Self { service }
    }
}
