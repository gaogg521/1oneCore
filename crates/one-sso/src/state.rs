//! Router state for one-sso routes.

use std::sync::Arc;

use crate::enterprise::EnterpriseSync;
use crate::service::SsoService;

#[derive(Clone)]
pub struct OneSsoRouterState {
    pub service: Arc<SsoService>,
    /// Syncs the SSO user's company + membership into the enterprise-org domain
    /// (one-enterprise), when the app layer wires one in. `None` — personal
    /// edition, WebUI-only builds, unit tests — means SSO login authenticates
    /// and nothing else, exactly as before the enterprise dimension existed.
    pub enterprise_sync: Option<Arc<dyn EnterpriseSync>>,
}

impl OneSsoRouterState {
    pub fn new(service: Arc<SsoService>) -> Self {
        Self {
            service,
            enterprise_sync: None,
        }
    }

    pub fn with_enterprise_sync(mut self, sync: Arc<dyn EnterpriseSync>) -> Self {
        self.enterprise_sync = Some(sync);
        self
    }
}
