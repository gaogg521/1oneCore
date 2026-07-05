//! Router state for one-devops routes.

use std::sync::Arc;

use crate::service::DevopsService;

#[derive(Clone)]
pub struct OneDevopsRouterState {
    pub service: Arc<DevopsService>,
}

impl OneDevopsRouterState {
    pub fn new(service: Arc<DevopsService>) -> Self {
        Self { service }
    }
}
