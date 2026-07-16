//! Router state for one-sso routes.

use std::sync::Arc;

use crate::enterprise::EnterpriseAutoJoiner;
use crate::service::SsoService;

#[derive(Clone)]
pub struct OneSsoRouterState {
    pub service: Arc<SsoService>,
    /// Joins an SSO user to the enterprise bound to their company, when the app
    /// layer wires one in. `None` — personal edition, WebUI-only builds, unit
    /// tests — means SSO login authenticates and nothing else, exactly as it
    /// behaved before the enterprise tier existed.
    pub auto_joiner: Option<Arc<dyn EnterpriseAutoJoiner>>,
}

impl OneSsoRouterState {
    pub fn new(service: Arc<SsoService>) -> Self {
        Self {
            service,
            auto_joiner: None,
        }
    }

    pub fn with_auto_joiner(mut self, joiner: Arc<dyn EnterpriseAutoJoiner>) -> Self {
        self.auto_joiner = Some(joiner);
        self
    }
}
