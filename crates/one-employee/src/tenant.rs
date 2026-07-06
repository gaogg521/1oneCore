//! Tenant resolution for team-shared employees (A1 L3).
//!
//! Kept as a trait so one-employee (and one-devops, which depends on it) do
//! not take a hard dependency on one-org. The app layer implements it over
//! `OrgService::tenant_of`. When no resolver is wired (personal edition,
//! unit tests) every user resolves to the `default` tenant.

use async_trait::async_trait;

/// The personal-edition tenant every user falls back to when no enterprise
/// membership (or resolver) is present.
pub const DEFAULT_TENANT: &str = "default";

#[async_trait]
pub trait TenantResolver: Send + Sync {
    /// The tenant the user belongs to. Implementations should return the
    /// personal `default` tenant for users without an enterprise membership.
    async fn tenant_of(&self, user_id: &str) -> String;
}
