//! Router state for one-sso routes.

use std::sync::Arc;

use crate::enterprise::{CompanyAdminCheck, EnterpriseSync};
use crate::service::SsoService;

#[derive(Clone)]
pub struct OneSsoRouterState {
    pub service: Arc<SsoService>,
    /// Syncs the SSO user's company + membership into the enterprise-org domain
    /// (one-enterprise), when the app layer wires one in. `None` — personal
    /// edition, WebUI-only builds, unit tests — means SSO login authenticates
    /// and nothing else, exactly as before the enterprise dimension existed.
    pub enterprise_sync: Option<Arc<dyn EnterpriseSync>>,
    /// Lets `RequireSsoAdmin` accept a company administrator (Direction B: SSO
    /// config is a company-level policy). `None` falls back to the project-group
    /// `one_user_org` admin check — personal / standalone behaviour is unchanged.
    pub company_admin_check: Option<Arc<dyn CompanyAdminCheck>>,
}

impl OneSsoRouterState {
    pub fn new(service: Arc<SsoService>) -> Self {
        Self {
            service,
            enterprise_sync: None,
            company_admin_check: None,
        }
    }

    pub fn with_enterprise_sync(mut self, sync: Arc<dyn EnterpriseSync>) -> Self {
        self.enterprise_sync = Some(sync);
        self
    }

    pub fn with_company_admin_check(mut self, check: Arc<dyn CompanyAdminCheck>) -> Self {
        self.company_admin_check = Some(check);
        self
    }
}
