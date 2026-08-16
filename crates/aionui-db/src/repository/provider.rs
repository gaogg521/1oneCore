use crate::error::DbError;
use crate::models::Provider;

/// Model provider data access abstraction.
///
/// Provides CRUD operations on the `providers` table.
/// API keys are stored encrypted; callers handle encryption/decryption.
#[async_trait::async_trait]
pub trait IProviderRepository: Send + Sync {
    /// Returns all providers, ordered by creation time ascending.
    ///
    /// ⚠️ **`user_id` does not scope the query in this fork.** Upstream
    /// (`7f8ed6c5`) made `providers` per-account; we deliberately kept the table
    /// deployment-global, because this fork's server deployment is "the
    /// operator configures the company's keys once and every member reaching
    /// this backend uses them". Scoping per account would show every existing
    /// member an empty model list the moment they upgrade.
    ///
    /// The parameter is kept so upstream's call sites keep merging cleanly, and
    /// because it is the natural place to reintroduce scoping if that decision
    /// is ever reversed. Do NOT "fix" it by adding the `WHERE user_id = ?` back
    /// without also solving how members get models — the failure is silent
    /// (empty list, no error). `provider_scope_is_deployment_global` in
    /// `sqlite_provider.rs` locks this.
    ///
    /// Members still cannot read the operator's API key out: redaction lives at
    /// the HTTP boundary (`aionui-system`'s `may_see_provider_secrets`).
    /// Genuinely per-member credentials go through enterprise model channels
    /// (`managed_by='enterprise'`, migration 041), not through this column.
    async fn list(&self, user_id: &str) -> Result<Vec<Provider>, DbError>;

    /// Finds a provider by ID, or `None` if not found.
    ///
    /// ⚠️ `user_id` does not scope the lookup — see [`Self::list`].
    async fn find_by_id(&self, user_id: &str, id: &str) -> Result<Option<Provider>, DbError>;

    /// Creates a new provider and returns the inserted row.
    ///
    /// `params.user_id` IS written to the row (so the column stays populated and
    /// the schema honest); it just does not gate later reads.
    async fn create(&self, params: CreateProviderParams<'_>) -> Result<Provider, DbError>;

    /// Updates an existing provider. Returns `DbError::NotFound` if the ID doesn't exist.
    ///
    /// ⚠️ `user_id` does not scope the update — see [`Self::list`].
    async fn update(&self, user_id: &str, id: &str, params: UpdateProviderParams<'_>) -> Result<Provider, DbError>;

    /// Deletes a provider by ID. Returns `DbError::NotFound` if the ID doesn't exist.
    ///
    /// ⚠️ `user_id` does not scope the delete — see [`Self::list`].
    async fn delete(&self, user_id: &str, id: &str) -> Result<(), DbError>;
}

/// Parameters for creating a new provider.
#[derive(Debug)]
pub struct CreateProviderParams<'a> {
    /// Optional caller-supplied id. When `None`, the repository generates one.
    pub id: Option<&'a str>,
    pub user_id: &'a str,
    pub platform: &'a str,
    pub name: &'a str,
    pub base_url: &'a str,
    pub api_key_encrypted: &'a str,
    pub models: &'a str,
    pub enabled: bool,
    pub capabilities: &'a str,
    pub context_limit: Option<i64>,
    pub model_protocols: Option<&'a str>,
    pub model_enabled: Option<&'a str>,
    pub model_health: Option<&'a str>,
    pub model_settings: &'a str,
    pub bedrock_config: Option<&'a str>,
    pub is_full_url: bool,
    /// `None` = user-configured. `Some("enterprise")` = materialized from a
    /// company model channel, and therefore read-only on this machine.
    pub managed_by: Option<&'a str>,
}

/// Parameters for updating an existing provider.
///
/// All fields are optional; `None` means "keep the current value".
#[derive(Debug, Default)]
pub struct UpdateProviderParams<'a> {
    pub platform: Option<&'a str>,
    pub name: Option<&'a str>,
    pub base_url: Option<&'a str>,
    pub api_key_encrypted: Option<&'a str>,
    pub models: Option<&'a str>,
    pub enabled: Option<bool>,
    pub capabilities: Option<&'a str>,
    pub context_limit: Option<Option<i64>>,
    pub model_protocols: Option<Option<&'a str>>,
    pub model_enabled: Option<Option<&'a str>>,
    pub model_health: Option<Option<&'a str>>,
    pub model_settings: Option<&'a str>,
    pub bedrock_config: Option<Option<&'a str>>,
    pub is_full_url: Option<bool>,
}
