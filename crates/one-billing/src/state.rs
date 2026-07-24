//! Router state for one-billing routes.

use std::sync::Arc;

use crate::service::BillingService;

#[derive(Clone)]
pub struct OneBillingRouterState {
    pub service: Arc<BillingService>,
}

impl OneBillingRouterState {
    pub fn new(service: Arc<BillingService>) -> Self {
        Self { service }
    }
}
