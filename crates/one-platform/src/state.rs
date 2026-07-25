//! Router state for one-platform routes.

use std::sync::Arc;

use crate::service::PlatformService;

#[derive(Clone)]
pub struct OnePlatformRouterState {
    pub service: Arc<PlatformService>,
}

impl OnePlatformRouterState {
    pub fn new(service: Arc<PlatformService>) -> Self {
        Self { service }
    }
}
