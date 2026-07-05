//! Router state for one-devops routes.

use std::sync::Arc;

use one_employee::EmployeeService;

use crate::service::DevopsService;

#[derive(Clone)]
pub struct OneDevopsRouterState {
    pub service: Arc<DevopsService>,
    /// Optional so unit tests can build state without the employee runtime.
    /// The dispatch endpoint returns a clear error when it is absent.
    pub employee: Option<Arc<EmployeeService>>,
}

impl OneDevopsRouterState {
    pub fn new(service: Arc<DevopsService>) -> Self {
        Self { service, employee: None }
    }

    /// Wire the employee runtime so requirements can be dispatched to digital
    /// employees. Called by the app router after the EmployeeService is built.
    pub fn with_employee(mut self, employee: Arc<EmployeeService>) -> Self {
        self.employee = Some(employee);
        self
    }
}
