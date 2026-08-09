//! Router state for one-org routes.

use std::sync::Arc;

use crate::bridge::CompanyAdminResolver;
use crate::directory_bridge::DirectoryTreeSource;
use crate::service::OrgService;

#[derive(Clone)]
pub struct OneOrgRouterState {
    pub service: Arc<OrgService>,
    /// Optional bridge to the company tier: lets a company admin create/list the
    /// project groups their company owns. `None` in personal edition / tests —
    /// company-scoped tenant routes then reject with a plain forbidden.
    pub company_resolver: Option<Arc<dyn CompanyAdminResolver>>,
    /// Optional bridge to the company directory mirror (T6 stage 3): lets an
    /// admin map a directory subtree into this project group's department
    /// tree. `None` in personal edition / tests — the picker and mapping
    /// routes then report "nothing to map" rather than failing. Router-state
    /// level, not a field on `OrgService`, because one-enterprise is
    /// constructed AFTER one-org in `aionui-app` — baking this into the
    /// service's own constructor would create a construction-order cycle
    /// with `EnterpriseService::with_session_revoker`, which needs an
    /// already-built `OrgService` the other way around.
    pub directory_source: Option<Arc<dyn DirectoryTreeSource>>,
}

impl OneOrgRouterState {
    pub fn new(service: Arc<OrgService>) -> Self {
        Self {
            service,
            company_resolver: None,
            directory_source: None,
        }
    }

    pub fn with_company_admin_resolver(mut self, resolver: Arc<dyn CompanyAdminResolver>) -> Self {
        self.company_resolver = Some(resolver);
        self
    }

    pub fn with_directory_source(mut self, source: Arc<dyn DirectoryTreeSource>) -> Self {
        self.directory_source = Some(source);
        self
    }
}
