//! Router state for one-employee routes.

use std::sync::Arc;

use crate::service::EmployeeService;
use crate::tenant::{DEFAULT_TENANT, TenantResolver};

#[derive(Clone)]
pub struct OneEmployeeRouterState {
    pub service: Arc<EmployeeService>,
    /// Optional so personal-edition deployments and unit tests can build state
    /// without one-org. Absent → every user is in the `default` tenant.
    pub tenant_resolver: Option<Arc<dyn TenantResolver>>,
}

impl OneEmployeeRouterState {
    pub fn new(service: Arc<EmployeeService>) -> Self {
        Self {
            service,
            tenant_resolver: None,
        }
    }

    /// Wire the tenant resolver so employees can be shared within a tenant.
    pub fn with_tenant_resolver(mut self, resolver: Arc<dyn TenantResolver>) -> Self {
        self.tenant_resolver = Some(resolver);
        self
    }

    /// Resolve the caller's tenant, falling back to the personal `default`
    /// tenant when no resolver is wired.
    pub async fn tenant_of(&self, user_id: &str) -> String {
        match &self.tenant_resolver {
            Some(resolver) => resolver.tenant_of(user_id).await,
            None => DEFAULT_TENANT.to_owned(),
        }
    }
}
