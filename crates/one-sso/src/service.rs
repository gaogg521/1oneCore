//! SSO orchestration service.
//!
//! Translation of 1ONE TS `ssoJitProvisioning.ts` + `oauthLoginHelpers.ts` +
//! the per-provider HTTP code, rebuilt on upstream primitives:
//!
//! - identity lookup → `one_sso_identities` table
//! - user creation → upstream `IUserRepository::create_user` (no password;
//!   SSO users get a random password they'll never know)
//! - password hashing → `aionui_auth::hash_password`
//! - session issue → `JwtService::sign` + `CookieConfig::build_session_cookie`
//!
//! State (OAuth `state` param) is kept in-memory — same approach as the TS
//! reference's `oauthLoginState.ts`. A single-process deployment is fine;
//! multi-instance would need a shared store, which M4 will introduce if
//! needed.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aionui_auth::{CookieConfig, JwtService, hash_password, generate_random_secret_string};
use aionui_common::now_ms;
use aionui_db::IUserRepository;
use sqlx::SqlitePool;
use tokio::sync::Mutex;

use crate::error::SsoError;
use crate::models::{SsoIdentityRow, SsoProviderConfigDto, SsoProviderKind, SsoProviderRow};
use crate::providers::{ProviderUserInfo, feishu::FeishuProviderConfig};

/// Lifetime of an OAuth `state` nonce — same as the TS reference (10 min).
const STATE_TTL: Duration = Duration::from_secs(10 * 60);

/// In-memory OAuth state store. Single-process only; see module docs.
#[derive(Clone)]
pub struct OAuthStateStore {
    inner: Arc<Mutex<HashMap<String, OAuthStateEntry>>>,
}

#[derive(Clone)]
pub struct OAuthStateEntry {
    pub provider: SsoProviderKind,
    pub redirect_target: Option<String>,
    pub desktop: bool,
    issued_at: Instant,
}

impl OAuthStateStore {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn issue(
        &self,
        provider: SsoProviderKind,
        redirect_target: Option<String>,
        desktop: bool,
    ) -> String {
        let state = uuid::Uuid::now_v7().simple().to_string();
        let entry = OAuthStateEntry {
            provider,
            redirect_target,
            desktop,
            issued_at: Instant::now(),
        };
        let mut map = self.inner.lock().await;
        map.insert(state.clone(), entry);
        map.retain(|_, e| e.issued_at.elapsed() < STATE_TTL);
        state
    }

    pub async fn consume(&self, state: &str) -> Option<OAuthStateEntry> {
        let mut map = self.inner.lock().await;
        map.remove(state)
    }
}

impl Default for OAuthStateStore {
    fn default() -> Self {
        Self::new()
    }
}

pub struct SsoService {
    pool: SqlitePool,
    user_repo: Arc<dyn IUserRepository>,
    jwt_service: Arc<JwtService>,
    cookie_config: Arc<CookieConfig>,
    state_store: OAuthStateStore,
}

/// Result of a successful SSO callback — the caller (route handler) wraps
/// this into a Set-Cookie + JSON response.
pub struct SsoSession {
    pub token: String,
    pub cookie: String,
    pub user_id: String,
    pub username: String,
    pub redirect_target: Option<String>,
    pub desktop: bool,
}

impl SsoService {
    pub fn new(
        pool: SqlitePool,
        user_repo: Arc<dyn IUserRepository>,
        jwt_service: Arc<JwtService>,
        cookie_config: Arc<CookieConfig>,
    ) -> Self {
        Self {
            pool,
            user_repo,
            jwt_service,
            cookie_config,
            state_store: OAuthStateStore::new(),
        }
    }

    pub fn state_store(&self) -> &OAuthStateStore {
        &self.state_store
    }

    /// Load a provider config row. Returns `ProviderNotConfigured` when no
    /// row exists.
    pub async fn get_provider_row(&self, provider: SsoProviderKind) -> Result<Option<SsoProviderRow>, SsoError> {
        let row = sqlx::query_as::<_, SsoProviderRow>(
            "SELECT provider, enabled, config, updated_at, updated_by FROM one_sso_providers WHERE provider = ?",
        )
        .bind(provider.as_str())
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// Public status list for the login page (secrets stripped).
    pub async fn list_provider_status(&self) -> Result<Vec<(String, bool, bool)>, SsoError> {
        let rows = sqlx::query_as::<_, SsoProviderRow>(
            "SELECT provider, enabled, config, updated_at, updated_by FROM one_sso_providers",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| {
                let configured = has_minimal_config(&row.provider, &row.config);
                (row.provider, row.enabled, configured)
            })
            .collect())
    }

    /// Admin-only status + non-secret config values, for pre-filling the
    /// settings form (BUG: the form used to always start blank because the
    /// only status endpoint stripped the *entire* config, secrets included —
    /// admins had to remember and retype App ID / Redirect URI on every
    /// edit). Secret fields are still stripped here.
    pub async fn list_provider_configs(&self) -> Result<Vec<SsoProviderConfigDto>, SsoError> {
        let rows = sqlx::query_as::<_, SsoProviderRow>(
            "SELECT provider, enabled, config, updated_at, updated_by FROM one_sso_providers",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| {
                let configured = has_minimal_config(&row.provider, &row.config);
                let config = redact_secret_fields(&row.provider, &row.config);
                SsoProviderConfigDto {
                    provider: row.provider,
                    enabled: row.enabled,
                    configured,
                    config,
                }
            })
            .collect())
    }

    pub async fn upsert_provider(
        &self,
        provider: SsoProviderKind,
        enabled: Option<bool>,
        config: Option<serde_json::Value>,
        updated_by: &str,
    ) -> Result<(), SsoError> {
        let existing = self.get_provider_row(provider).await?;
        let now = now_ms();
        match existing {
            Some(row) => {
                let new_enabled = enabled.unwrap_or(row.enabled);
                // Merge incoming keys into the stored config instead of
                // replacing it wholesale. Secrets are never echoed to the
                // admin form, so the client only sends the fields the user
                // just (re)typed; a wholesale replace would wipe every
                // untouched field (e.g. appSecret / redirectUri) and make the
                // config impossible to edit incrementally. Blank fields are
                // dropped client-side, so "leave empty to keep" holds.
                let new_config = match config {
                    Some(incoming) => merge_config(&row.config, incoming),
                    None => row.config,
                };
                sqlx::query(
                    "UPDATE one_sso_providers SET enabled = ?, config = ?, updated_at = ?, updated_by = ? WHERE provider = ?",
                )
                .bind(new_enabled)
                .bind(&new_config)
                .bind(now)
                .bind(updated_by)
                .bind(provider.as_str())
                .execute(&self.pool)
                .await?;
            }
            None => {
                let config_str = config
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "{}".into());
                let enabled_val = enabled.unwrap_or(false);
                sqlx::query(
                    "INSERT INTO one_sso_providers (provider, enabled, config, updated_at, updated_by) \
                     VALUES (?, ?, ?, ?, ?)",
                )
                .bind(provider.as_str())
                .bind(enabled_val)
                .bind(&config_str)
                .bind(now)
                .bind(updated_by)
                .execute(&self.pool)
                .await?;
            }
        }
        Ok(())
    }

    /// JIT: look up the identity; if missing, create a user with a random
    /// password and bind the identity. Returns the local user id + username.
    pub async fn resolve_or_provision_user(
        &self,
        provider: SsoProviderKind,
        profile: ProviderUserInfo,
    ) -> Result<(String, String, bool), SsoError> {
        let external_id = profile.external_id.trim();
        if external_id.is_empty() {
            return Err(SsoError::IdentityMissing);
        }

        // 1. Existing identity binding → reuse user.
        if let Some(identity) = self.find_identity(provider, external_id).await? {
            let user = self
                .user_repo
                .find_by_id(&identity.user_id)
                .await
                .map_err(|e| SsoError::Internal(format!("find user: {e}")))?
                .ok_or_else(|| SsoError::Internal("identity points to missing user".into()))?;
            self.touch_identity(provider, external_id).await;
            return Ok((user.id, user.username, false));
        }

        // 2. No binding → provision a new user with a random password.
        let username = allocate_unique_username(&profile.preferred_username, &self.user_repo).await?;
        let random_password = generate_random_secret_string();
        let password_hash = hash_password(&random_password)?;
        let user = self
            .user_repo
            .create_user(&username, &password_hash)
            .await
            .map_err(|e| SsoError::Internal(format!("create_user: {e}")))?;
        self.bind_identity(provider, external_id, &user.id).await?;
        Ok((user.id, user.username, true))
    }

    /// Sign a JWT + build the session cookie. Mirrors the upstream
    /// `login_handler` shape so CSRF/QR-login inheritance stays intact.
    pub fn issue_session(
        &self,
        user_id: &str,
        username: &str,
        redirect_target: Option<String>,
        desktop: bool,
    ) -> Result<SsoSession, SsoError> {
        let token = self
            .jwt_service
            .sign(user_id, username)
            .map_err(|e| SsoError::Internal(format!("token sign: {e}")))?;
        let cookie = self.cookie_config.build_session_cookie(&token);
        Ok(SsoSession {
            token,
            cookie,
            user_id: user_id.to_owned(),
            username: username.to_owned(),
            redirect_target,
            desktop,
        })
    }

    async fn find_identity(
        &self,
        provider: SsoProviderKind,
        external_id: &str,
    ) -> Result<Option<SsoIdentityRow>, SsoError> {
        let row = sqlx::query_as::<_, SsoIdentityRow>(
            "SELECT id, provider, external_id, user_id, tenant_id, last_seen_at, created_at \
             FROM one_sso_identities WHERE provider = ? AND external_id = ?",
        )
        .bind(provider.as_str())
        .bind(external_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    async fn bind_identity(
        &self,
        provider: SsoProviderKind,
        external_id: &str,
        user_id: &str,
    ) -> Result<(), SsoError> {
        let id = uuid::Uuid::now_v7().simple().to_string();
        let now = now_ms();
        sqlx::query(
            "INSERT INTO one_sso_identities (id, provider, external_id, user_id, tenant_id, created_at, last_seen_at) \
             VALUES (?, ?, ?, ?, 'default', ?, ?)",
        )
        .bind(&id)
        .bind(provider.as_str())
        .bind(external_id)
        .bind(user_id)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn touch_identity(&self, provider: SsoProviderKind, external_id: &str) {
        let _ = sqlx::query(
            "UPDATE one_sso_identities SET last_seen_at = ? WHERE provider = ? AND external_id = ?",
        )
        .bind(now_ms())
        .bind(provider.as_str())
        .bind(external_id)
        .execute(&self.pool)
        .await;
    }
}

/// Allocate a unique username, falling back to `provider_ext123` shape
/// when the preferred name is taken. Mirrors the TS `allocateUniqueUsername`.
async fn allocate_unique_username(
    preferred: &str,
    user_repo: &Arc<dyn IUserRepository>,
) -> Result<String, SsoError> {
    let base = sanitize_username(preferred);
    let base = if base.is_empty() {
        format!("sso_{}", &uuid::Uuid::now_v7().simple().to_string()[..8])
    } else {
        base
    };

    // Fast path: base is free.
    if user_repo
        .find_by_username(&base)
        .await
        .map_err(|e| SsoError::Internal(format!("find_by_username: {e}")))?
        .is_none()
    {
        return Ok(base);
    }

    for attempt in 1..=100 {
        let candidate = format!("{base}_{attempt}");
        if user_repo
            .find_by_username(&candidate)
            .await
            .map_err(|e| SsoError::Internal(format!("find_by_username: {e}")))?
            .is_none()
        {
            return Ok(candidate);
        }
    }
    Ok(format!(
        "{base}_{}",
        &uuid::Uuid::now_v7().simple().to_string()[..6]
    ))
}

/// Lower-case, ASCII-clean username. Non-ASCII display names fall back to
/// a `sso_` prefix so we don't put raw unicode into the upstream users table.
fn sanitize_username(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if !trimmed.is_ascii() {
        return String::new();
    }
    let lowered = trimmed.to_ascii_lowercase();
    let cleaned: String = lowered
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '@' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches('_').to_owned();
    if cleaned.len() >= 2 {
        cleaned.chars().take(64).collect()
    } else {
        String::new()
    }
}

/// Merge `incoming` config keys onto the `existing` stored JSON object,
/// incoming values winning. Non-object inputs fall back gracefully: a
/// non-object `incoming` replaces (mirrors the old behavior), and a
/// non-object/empty `existing` is treated as `{}`. Enables incremental
/// edits from the admin form, which only sends the fields just typed.
fn merge_config(existing: &str, incoming: serde_json::Value) -> String {
    let serde_json::Value::Object(incoming_obj) = incoming else {
        // Not an object — nothing sensible to merge; store as-is.
        return incoming.to_string();
    };
    let mut merged = serde_json::from_str::<serde_json::Value>(existing)
        .ok()
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    for (key, value) in incoming_obj {
        merged.insert(key, value);
    }
    serde_json::Value::Object(merged).to_string()
}

/// Minimal-config check per provider — used by the login page to decide
/// whether to render the SSO button at all (no secrets exposed).
fn has_minimal_config(provider: &str, config_json: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(config_json) else {
        return false;
    };
    let obj = match value.as_object() {
        Some(o) => o,
        None => return false,
    };
    let has_non_empty = |key: &str| {
        obj.get(key)
            .and_then(|v| v.as_str())
            .map(|s| !s.trim().is_empty() && s != "******")
            .unwrap_or(false)
    };
    match provider {
        "feishu" => has_non_empty("appId") && has_non_empty("appSecret"),
        "dingtalk" => has_non_empty("appKey") && has_non_empty("appSecret"),
        "wecom" => has_non_empty("corpId") && has_non_empty("secret"),
        "ldap" => obj
            .get("url")
            .and_then(|v| v.as_str())
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false),
        _ => false,
    }
}

/// Field names holding secrets per provider — never sent to the admin
/// settings form, even redacted. Mirrors the `secret: true` markers in the
/// frontend `SsoSettingsTab` field specs.
fn secret_keys(provider: &str) -> &'static [&'static str] {
    match provider {
        "feishu" => &["appSecret"],
        "dingtalk" => &["appSecret"],
        "wecom" => &["secret"],
        "ldap" => &["bindPassword"],
        _ => &[],
    }
}

/// Strip secret fields from a stored config JSON, keeping the rest so the
/// admin form can pre-fill non-secret values (App ID, Redirect URI, ...).
fn redact_secret_fields(provider: &str, config_json: &str) -> serde_json::Value {
    let mut obj = serde_json::from_str::<serde_json::Value>(config_json)
        .ok()
        .and_then(|v| match v {
            serde_json::Value::Object(o) => Some(o),
            _ => None,
        })
        .unwrap_or_default();
    for key in secret_keys(provider) {
        obj.remove(*key);
    }
    serde_json::Value::Object(obj)
}

/// Parse a Feishu config row into a typed config, applying env-var fallbacks
/// the way the TS reference does. Returns `None` when the row is missing or
/// has no appId.
pub fn parse_feishu_config(row: &SsoProviderRow) -> Option<FeishuProviderConfig> {
    let value: serde_json::Value = serde_json::from_str(&row.config).unwrap_or_default();
    let app_id = value
        .get("appId")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .filter(|s| !s.is_empty())?;
    let app_secret = value
        .get("appSecret")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .unwrap_or_default();
    let redirect_uri = value
        .get("redirectUri")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .unwrap_or_default();
    let external_id_field = value
        .get("externalIdField")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .unwrap_or_else(|| "union_id".into());
    Some(FeishuProviderConfig {
        app_id,
        app_secret,
        redirect_uri,
        external_id_field,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_username_keeps_ascii_alphanum() {
        assert_eq!(sanitize_username("Zhang.San_2024"), "zhang.san_2024");
    }

    #[test]
    fn sanitize_username_replaces_unsafe_chars() {
        assert_eq!(sanitize_username("张三!"), "");
    }

    #[test]
    fn sanitize_username_trims_underscores() {
        assert_eq!(sanitize_username("__hello__"), "hello");
    }

    #[test]
    fn sanitize_username_truncates_to_64() {
        let long = "a".repeat(80);
        let result = sanitize_username(&long);
        assert_eq!(result.len(), 64);
    }

    #[test]
    fn merge_config_keeps_untouched_fields() {
        // BUG5: admin re-saves feishu with only appId changed (secret not
        // echoed, so the form only sends appId). The stored appSecret /
        // redirectUri must survive the partial update.
        let existing = r#"{"appId":"cli_old","appSecret":"s3cret","redirectUri":"https://x/cb"}"#;
        let incoming = serde_json::json!({ "appId": "cli_new" });
        let merged = merge_config(existing, incoming);
        let value: serde_json::Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(value["appId"], "cli_new");
        assert_eq!(value["appSecret"], "s3cret");
        assert_eq!(value["redirectUri"], "https://x/cb");
    }

    #[test]
    fn merge_config_from_empty_existing() {
        let merged = merge_config("", serde_json::json!({ "appId": "cli_a" }));
        let value: serde_json::Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(value["appId"], "cli_a");
    }

    #[test]
    fn has_minimal_config_feishu_requires_both_fields() {
        assert!(has_minimal_config(
            "feishu",
            r#"{"appId":"cli_x","appSecret":"secret"}"#
        ));
        assert!(!has_minimal_config("feishu", r#"{"appId":"cli_x"}"#));
        assert!(!has_minimal_config(
            "feishu",
            r#"{"appId":"cli_x","appSecret":"******"}"#
        ));
    }

    #[test]
    fn redact_secret_fields_strips_only_secrets() {
        let config = r#"{"appId":"cli_x","appSecret":"s3cret","redirectUri":"https://x/cb"}"#;
        let redacted = redact_secret_fields("feishu", config);
        assert_eq!(redacted["appId"], "cli_x");
        assert_eq!(redacted["redirectUri"], "https://x/cb");
        assert!(redacted.get("appSecret").is_none());
    }

    #[test]
    fn redact_secret_fields_covers_every_provider_secret() {
        assert!(
            redact_secret_fields("dingtalk", r#"{"appKey":"k","appSecret":"s"}"#)
                .get("appSecret")
                .is_none()
        );
        assert!(
            redact_secret_fields("wecom", r#"{"corpId":"c","secret":"s"}"#)
                .get("secret")
                .is_none()
        );
        assert!(
            redact_secret_fields("ldap", r#"{"url":"ldap://x","bindPassword":"p"}"#)
                .get("bindPassword")
                .is_none()
        );
    }

    #[test]
    fn redact_secret_fields_handles_empty_config() {
        let redacted = redact_secret_fields("feishu", "");
        assert_eq!(redacted, serde_json::json!({}));
    }

    #[test]
    fn parse_feishu_config_reads_fields() {
        let row = SsoProviderRow {
            provider: "feishu".into(),
            enabled: true,
            config: r#"{"appId":"cli_a","appSecret":"s","redirectUri":"https://x/cb","externalIdField":"open_id"}"#.to_owned(),
            updated_at: 0,
            updated_by: None,
        };
        let cfg = parse_feishu_config(&row).unwrap();
        assert_eq!(cfg.app_id, "cli_a");
        assert_eq!(cfg.external_id_field, "open_id");
    }

    #[test]
    fn parse_feishu_config_returns_none_without_app_id() {
        let row = SsoProviderRow {
            provider: "feishu".into(),
            enabled: false,
            config: "{}".into(),
            updated_at: 0,
            updated_by: None,
        };
        assert!(parse_feishu_config(&row).is_none());
    }

    #[tokio::test]
    async fn state_store_issues_and_consumes() {
        let store = OAuthStateStore::new();
        let state = store.issue(SsoProviderKind::Feishu, Some("/guid".into()), false).await;
        let entry = store.consume(&state).await.expect("state should be present");
        assert_eq!(entry.provider, SsoProviderKind::Feishu);
        assert_eq!(entry.redirect_target.as_deref(), Some("/guid"));
        // Second consume returns None.
        assert!(store.consume(&state).await.is_none());
    }
}
