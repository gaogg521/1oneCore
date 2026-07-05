//! Router state for one-employee routes.

use std::sync::Arc;

use crate::service::EmployeeService;

#[derive(Clone)]
pub struct OneEmployeeRouterState {
    pub service: Arc<EmployeeService>,
}

impl OneEmployeeRouterState {
    pub fn new(service: Arc<EmployeeService>) -> Self {
        Self { service }
    }
}
