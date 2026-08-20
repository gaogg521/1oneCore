//! Company (真实企业 / one-enterprise) seat-sync hook for project-group joins.
//!
//! one-org and one-enterprise are same-layer domain crates and must not depend
//! on each other (workspace `AGENTS.md` § Crate Hierarchy). This trait lets a
//! successful project-group join also register the joiner as a company member
//! when the group belongs to one, without one-org depending on one-enterprise;
//! the app layer implements it over `one_enterprise::EnterpriseService`.
//!
//! # Why this exists
//!
//! A project group created under a company (`one_tenants.enterprise_id`) hands
//! out its own invite codes, and joining one has never touched
//! `one_enterprise_members` — only SSO login does, via the separate
//! `EnterpriseSync` hook. That left a gap: a company admin can invite an
//! unbounded number of people into "their" project groups (who then use the
//! company's model channels / team resources), and not one of them shows up
//! in the company's "成员" list or counts against its seat limit — the
//! subscription's whole point silently does not apply to this path.
//!
//! When no hook is wired (personal edition, unit tests, or a group with no
//! `enterprise_id`) a project-group join behaves exactly as before — nothing
//! else happens.

use async_trait::async_trait;

#[async_trait]
pub trait CompanySeatSync: Send + Sync {
    /// Called after a user successfully joins a project group belonging to
    /// `enterprise_id`. Implementations ensure a `one_enterprise_members` row
    /// exists for them, using the same active/pending seat-cap rule as SSO
    /// auto-provisioning (idempotent — an existing ACTIVE row is untouched).
    ///
    /// Best-effort and **must never fail the join**: a user who successfully
    /// joined the project group must still get that result even if the
    /// company-side sync cannot complete. Implementations swallow their own
    /// errors.
    async fn ensure_company_member(&self, user_id: &str, enterprise_id: &str, display_name: Option<&str>);

    /// The other direction of `ensure_company_member`: called when leaving
    /// or being removed from a project group empties out the user's LAST
    /// group under `enterprise_id` — the seat this company issued them
    /// (via project-group invite, not SSO) has nothing left to be attached
    /// to. Without this, self-service "退出项目组" never released the seat:
    /// `OrgService::leave`/`remove_member` only ever touched `one_user_org`,
    /// so a company kept billing/counting someone who had quietly left every
    /// group under it, until an admin happened to notice and remove them
    /// separately from the company console.
    ///
    /// Best-effort and **must never fail the caller's leave/removal** — same
    /// contract as `ensure_company_member`. Implementations swallow their
    /// own errors, including "this user was never a company member" (they
    /// may have joined by invite code with no SSO identity at all).
    async fn release_company_member(&self, user_id: &str, enterprise_id: &str);
}
