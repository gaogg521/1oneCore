//! Router state for one-org routes.

use std::sync::Arc;

use crate::bridge::CompanyAdminResolver;
use crate::service::OrgService;

#[derive(Clone)]
pub struct OneOrgRouterState {
    pub service: Arc<OrgService>,
    /// Optional bridge to the company tier: lets a company admin create/list the
    /// project groups their company owns. `None` in personal edition / tests —
    /// company-scoped tenant routes then reject with a plain forbidden.
    pub company_resolver: Option<Arc<dyn CompanyAdminResolver>>,
}

impl OneOrgRouterState {
    pub fn new(service: Arc<OrgService>) -> Self {
        Self {
            service,
            company_resolver: None,
        }
    }

    pub fn with_company_admin_resolver(mut self, resolver: Arc<dyn CompanyAdminResolver>) -> Self {
        self.company_resolver = Some(resolver);
        self
    }
}
