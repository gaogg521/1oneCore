//! Router state for one-devops routes.

use std::sync::Arc;

use one_employee::{DEFAULT_TENANT, EmployeeService, TenantResolver};

use crate::service::DevopsService;

#[derive(Clone)]
pub struct OneDevopsRouterState {
    pub service: Arc<DevopsService>,
    /// Optional so unit tests can build state without the employee runtime.
    /// The dispatch endpoint returns a clear error when it is absent.
    pub employee: Option<Arc<EmployeeService>>,
    /// Optional tenant resolver (A1 L3): lets dispatch/breakdown reach a
    /// team-shared employee owned by another same-tenant member. Absent →
    /// callers resolve to the `default` tenant (personal edition).
    pub tenant_resolver: Option<Arc<dyn TenantResolver>>,
}

impl OneDevopsRouterState {
    pub fn new(service: Arc<DevopsService>) -> Self {
        Self {
            service,
            employee: None,
            tenant_resolver: None,
        }
    }

    /// Wire the employee runtime so requirements can be dispatched to digital
    /// employees. Called by the app router after the EmployeeService is built.
    pub fn with_employee(mut self, employee: Arc<EmployeeService>) -> Self {
        self.employee = Some(employee);
        self
    }

    /// Wire the tenant resolver so dispatch/breakdown can use team-shared
    /// employees.
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
