//! Platform infrastructure config service (P1-3 container runtime + P2-2
//! realtime collaboration). All logic lives here; routes only transform
//! request/response.
//!
//! Both configs are per-project-group singletons keyed by tenant_id, with any
//! credential encrypted at rest (same helper as one-org's SMTP password).
//! Storing a config runs nothing — a real `ContainerRuntime` /
//! `CollaborationProvider` (wired at the app layer) does the actual work; until
//! then a probe reports "not configured" via the Noop defaults.

use std::sync::Arc;

use sqlx::SqlitePool;

use aionui_common::{decrypt_string, encrypt_string, now_ms};

use crate::collaboration::{
    CollaborationProvider, CollaborationSettings, CollaborationStatus, NoopCollaborationProvider,
};
use crate::container::{ContainerRuntime, ContainerSettings, ContainerStatus, NoopContainerRuntime};
use crate::error::PlatformError;
use crate::ip_allowlist::ip_allowed;
use crate::models::{CollaborationConfigDto, ContainerConfigDto, IpAllowlistConfigDto, SiemConfigDto};
use crate::siem::{NoopSiemExporter, SiemExporter, SiemSettings, SiemStatus};

/// The caller's resolved enterprise membership (active tenant + role).
#[derive(Debug, Clone)]
pub struct PlatformActor {
    pub tenant_id: String,
    pub role: String,
}

pub struct PlatformService {
    pool: SqlitePool,
    /// Encrypts stored registry/collaboration credentials (same key/helper as
    /// one-org's SMTP password and provider API keys).
    encryption_key: [u8; 32],
    container_runtime: Arc<dyn ContainerRuntime>,
    collaboration_provider: Arc<dyn CollaborationProvider>,
    siem_exporter: Arc<dyn SiemExporter>,
}

fn is_admin_role(role: &str) -> bool {
    matches!(role, "org_admin" | "system_admin" | "admin")
}

impl PlatformService {
    pub fn new(pool: SqlitePool, encryption_key: [u8; 32]) -> Self {
        Self {
            pool,
            encryption_key,
            container_runtime: Arc::new(NoopContainerRuntime),
            collaboration_provider: Arc::new(NoopCollaborationProvider),
            siem_exporter: Arc::new(NoopSiemExporter),
        }
    }

    /// Swap in a real `ContainerRuntime` once a container client is wired at the
    /// app layer. Chainable at construction time.
    pub fn with_container_runtime(mut self, runtime: Arc<dyn ContainerRuntime>) -> Self {
        self.container_runtime = runtime;
        self
    }

    /// Swap in a real `CollaborationProvider` once a backend is wired.
    pub fn with_collaboration_provider(mut self, provider: Arc<dyn CollaborationProvider>) -> Self {
        self.collaboration_provider = provider;
        self
    }

    /// Swap in a real `SiemExporter` once log forwarding is wired.
    pub fn with_siem_exporter(mut self, exporter: Arc<dyn SiemExporter>) -> Self {
        self.siem_exporter = exporter;
        self
    }

    /// Resolve the caller's active-tenant membership (tenant + role) from
    /// one-org's `one_user_org` table (same SQLite pool). `Ok(None)` when the
    /// user has no membership row — personal edition / standalone owner — or
    /// when the table itself does not exist (one-org never initialized). Role
    /// is scoped to the active tenant (active membership first, else
    /// most-recently-joined), mirroring `one_devops::user_org_role`.
    pub async fn resolve_actor(&self, user_id: &str) -> Result<Option<PlatformActor>, PlatformError> {
        let result = sqlx::query_as::<_, (String, String)>(
            "SELECT uo.tenant_id, uo.role FROM one_user_org uo WHERE uo.user_id = ? \
             ORDER BY (uo.tenant_id = (SELECT tenant_id FROM one_active_tenant WHERE user_id = uo.user_id)) DESC, \
                      uo.created_at DESC, uo.tenant_id ASC LIMIT 1",
        )
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await;
        match result {
            Ok(Some((tenant_id, role))) => Ok(Some(PlatformActor { tenant_id, role })),
            Ok(None) => Ok(None),
            // Table missing = one-org never initialized = standalone.
            Err(sqlx::Error::Database(e)) if e.message().contains("no such table") => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Resolve the caller and require enterprise membership with an admin role.
    /// Personal edition (no membership) → `NotInEnterprise`; non-admin →
    /// `Forbidden`.
    pub async fn require_admin(&self, user_id: &str) -> Result<PlatformActor, PlatformError> {
        match self.resolve_actor(user_id).await? {
            None => Err(PlatformError::NotInEnterprise),
            Some(actor) if !is_admin_role(&actor.role) => {
                Err(PlatformError::Forbidden("Administrator role required".into()))
            }
            Some(actor) => Ok(actor),
        }
    }

    // --- Container runtime config (P1-3) ---

    pub async fn get_container_config(&self, tenant_id: &str) -> Result<ContainerConfigDto, PlatformError> {
        type Row = (
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            bool,
            i64,
        );
        let row: Option<Row> = sqlx::query_as(
            "SELECT runtime_kind, endpoint, default_image, registry, registry_secret_encrypted, enabled, updated_at \
             FROM one_container_config WHERE tenant_id = ?",
        )
        .bind(tenant_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(match row {
            Some((runtime_kind, endpoint, default_image, registry, registry_secret_encrypted, enabled, updated_at)) => {
                ContainerConfigDto {
                    runtime_kind,
                    endpoint,
                    default_image,
                    registry,
                    has_registry_secret: registry_secret_encrypted.is_some(),
                    enabled,
                    updated_at: Some(updated_at),
                }
            }
            None => ContainerConfigDto {
                runtime_kind: None,
                endpoint: None,
                default_image: None,
                registry: None,
                has_registry_secret: false,
                enabled: false,
                updated_at: None,
            },
        })
    }

    /// `registry_secret` absent/empty = keep the stored one (if any).
    #[allow(clippy::too_many_arguments)]
    pub async fn set_container_config(
        &self,
        tenant_id: &str,
        runtime_kind: Option<&str>,
        endpoint: Option<&str>,
        default_image: Option<&str>,
        registry: Option<&str>,
        registry_secret: Option<&str>,
        enabled: bool,
    ) -> Result<ContainerConfigDto, PlatformError> {
        let existing: Option<String> =
            sqlx::query_scalar("SELECT registry_secret_encrypted FROM one_container_config WHERE tenant_id = ?")
                .bind(tenant_id)
                .fetch_optional(&self.pool)
                .await?
                .flatten();
        let secret_encrypted = match registry_secret {
            Some(s) if !s.is_empty() => {
                Some(encrypt_string(s, &self.encryption_key).map_err(|e| PlatformError::Internal(e.to_string()))?)
            }
            _ => existing,
        };
        sqlx::query(
            "INSERT INTO one_container_config \
                 (tenant_id, runtime_kind, endpoint, default_image, registry, registry_secret_encrypted, enabled, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(tenant_id) DO UPDATE SET runtime_kind = excluded.runtime_kind, endpoint = excluded.endpoint, \
                 default_image = excluded.default_image, registry = excluded.registry, \
                 registry_secret_encrypted = excluded.registry_secret_encrypted, enabled = excluded.enabled, \
                 updated_at = excluded.updated_at",
        )
        .bind(tenant_id)
        .bind(runtime_kind)
        .bind(endpoint)
        .bind(default_image)
        .bind(registry)
        .bind(&secret_encrypted)
        .bind(enabled)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        self.get_container_config(tenant_id).await
    }

    /// Decrypted registry credential, for a real `ContainerRuntime` to consume.
    /// `None` when unset or decryption fails (never panics).
    pub async fn container_registry_secret(&self, tenant_id: &str) -> Result<Option<String>, PlatformError> {
        let encrypted: Option<String> =
            sqlx::query_scalar("SELECT registry_secret_encrypted FROM one_container_config WHERE tenant_id = ?")
                .bind(tenant_id)
                .fetch_optional(&self.pool)
                .await?
                .flatten();
        Ok(encrypted.and_then(|e| decrypt_string(&e, &self.encryption_key).ok()))
    }

    /// Probe the container runtime through whatever is wired
    /// (`NoopContainerRuntime` by default — reports "not configured").
    pub async fn probe_container(&self, tenant_id: &str) -> Result<ContainerStatus, PlatformError> {
        let cfg = self.get_container_config(tenant_id).await?;
        let secret = self.container_registry_secret(tenant_id).await?;
        Ok(self
            .container_runtime
            .probe(ContainerSettings {
                runtime_kind: cfg.runtime_kind.as_deref(),
                endpoint: cfg.endpoint.as_deref(),
                default_image: cfg.default_image.as_deref(),
                registry: cfg.registry.as_deref(),
                registry_secret: secret.as_deref(),
            })
            .await)
    }

    // --- Realtime collaboration config (P2-2) ---

    pub async fn get_collaboration_config(&self, tenant_id: &str) -> Result<CollaborationConfigDto, PlatformError> {
        type Row = (Option<String>, Option<String>, Option<String>, bool, bool, i64);
        let row: Option<Row> = sqlx::query_as(
            "SELECT provider, endpoint, secret_encrypted, presence, enabled, updated_at \
             FROM one_collaboration_config WHERE tenant_id = ?",
        )
        .bind(tenant_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(match row {
            Some((provider, endpoint, secret_encrypted, presence, enabled, updated_at)) => CollaborationConfigDto {
                provider,
                endpoint,
                has_secret: secret_encrypted.is_some(),
                presence,
                enabled,
                updated_at: Some(updated_at),
            },
            None => CollaborationConfigDto {
                provider: None,
                endpoint: None,
                has_secret: false,
                presence: false,
                enabled: false,
                updated_at: None,
            },
        })
    }

    /// `secret` absent/empty = keep the stored one (if any).
    pub async fn set_collaboration_config(
        &self,
        tenant_id: &str,
        provider: Option<&str>,
        endpoint: Option<&str>,
        secret: Option<&str>,
        presence: bool,
        enabled: bool,
    ) -> Result<CollaborationConfigDto, PlatformError> {
        let existing: Option<String> =
            sqlx::query_scalar("SELECT secret_encrypted FROM one_collaboration_config WHERE tenant_id = ?")
                .bind(tenant_id)
                .fetch_optional(&self.pool)
                .await?
                .flatten();
        let secret_encrypted = match secret {
            Some(s) if !s.is_empty() => {
                Some(encrypt_string(s, &self.encryption_key).map_err(|e| PlatformError::Internal(e.to_string()))?)
            }
            _ => existing,
        };
        sqlx::query(
            "INSERT INTO one_collaboration_config \
                 (tenant_id, provider, endpoint, secret_encrypted, presence, enabled, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(tenant_id) DO UPDATE SET provider = excluded.provider, endpoint = excluded.endpoint, \
                 secret_encrypted = excluded.secret_encrypted, presence = excluded.presence, \
                 enabled = excluded.enabled, updated_at = excluded.updated_at",
        )
        .bind(tenant_id)
        .bind(provider)
        .bind(endpoint)
        .bind(&secret_encrypted)
        .bind(presence)
        .bind(enabled)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        self.get_collaboration_config(tenant_id).await
    }

    /// Decrypted collaboration credential, for a real `CollaborationProvider`.
    pub async fn collaboration_secret(&self, tenant_id: &str) -> Result<Option<String>, PlatformError> {
        let encrypted: Option<String> =
            sqlx::query_scalar("SELECT secret_encrypted FROM one_collaboration_config WHERE tenant_id = ?")
                .bind(tenant_id)
                .fetch_optional(&self.pool)
                .await?
                .flatten();
        Ok(encrypted.and_then(|e| decrypt_string(&e, &self.encryption_key).ok()))
    }

    /// Probe the collaboration backend (`NoopCollaborationProvider` by default).
    pub async fn probe_collaboration(&self, tenant_id: &str) -> Result<CollaborationStatus, PlatformError> {
        let cfg = self.get_collaboration_config(tenant_id).await?;
        let secret = self.collaboration_secret(tenant_id).await?;
        Ok(self
            .collaboration_provider
            .probe(CollaborationSettings {
                provider: cfg.provider.as_deref(),
                endpoint: cfg.endpoint.as_deref(),
                secret: secret.as_deref(),
                presence: cfg.presence,
            })
            .await)
    }

    // --- IP allowlist (P1-4) ---

    pub async fn get_ip_allowlist(&self, tenant_id: &str) -> Result<IpAllowlistConfigDto, PlatformError> {
        let row: Option<(Option<String>, bool, i64)> =
            sqlx::query_as("SELECT cidrs, enabled, updated_at FROM one_ip_allowlist_config WHERE tenant_id = ?")
                .bind(tenant_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(match row {
            Some((cidrs, enabled, updated_at)) => IpAllowlistConfigDto {
                cidrs: cidrs
                    .filter(|s| !s.trim().is_empty())
                    .and_then(|s| serde_json::from_str(&s).ok())
                    .unwrap_or_default(),
                enabled,
                updated_at: Some(updated_at),
            },
            None => IpAllowlistConfigDto {
                cidrs: Vec::new(),
                enabled: false,
                updated_at: None,
            },
        })
    }

    pub async fn set_ip_allowlist(
        &self,
        tenant_id: &str,
        cidrs: &[String],
        enabled: bool,
    ) -> Result<IpAllowlistConfigDto, PlatformError> {
        let cidrs_json = serde_json::to_string(cidrs).map_err(|e| PlatformError::Internal(e.to_string()))?;
        sqlx::query(
            "INSERT INTO one_ip_allowlist_config (tenant_id, cidrs, enabled, updated_at) VALUES (?, ?, ?, ?) \
             ON CONFLICT(tenant_id) DO UPDATE SET cidrs = excluded.cidrs, enabled = excluded.enabled, \
                 updated_at = excluded.updated_at",
        )
        .bind(tenant_id)
        .bind(&cidrs_json)
        .bind(enabled)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        self.get_ip_allowlist(tenant_id).await
    }

    /// Whether `ip` may reach `tenant_id`'s server per the allowlist. When the
    /// allowlist is disabled, everyone is allowed (the reserved default — no
    /// blocking). When enabled, the IP must match a configured CIDR/address.
    pub async fn is_ip_allowed(&self, tenant_id: &str, ip: &str) -> Result<bool, PlatformError> {
        let cfg = self.get_ip_allowlist(tenant_id).await?;
        Ok(!cfg.enabled || ip_allowed(&cfg.cidrs, ip))
    }

    // --- SIEM export (P1-4) ---

    pub async fn get_siem_config(&self, tenant_id: &str) -> Result<SiemConfigDto, PlatformError> {
        type SiemRow = (Option<String>, Option<String>, Option<String>, bool, i64);
        let row: Option<SiemRow> = sqlx::query_as(
            "SELECT kind, endpoint, secret_encrypted, enabled, updated_at FROM one_siem_config WHERE tenant_id = ?",
        )
        .bind(tenant_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(match row {
            Some((kind, endpoint, secret_encrypted, enabled, updated_at)) => SiemConfigDto {
                kind,
                endpoint,
                has_secret: secret_encrypted.is_some(),
                enabled,
                updated_at: Some(updated_at),
            },
            None => SiemConfigDto {
                kind: None,
                endpoint: None,
                has_secret: false,
                enabled: false,
                updated_at: None,
            },
        })
    }

    /// `secret` absent/empty = keep the stored one (if any).
    pub async fn set_siem_config(
        &self,
        tenant_id: &str,
        kind: Option<&str>,
        endpoint: Option<&str>,
        secret: Option<&str>,
        enabled: bool,
    ) -> Result<SiemConfigDto, PlatformError> {
        let existing: Option<String> =
            sqlx::query_scalar("SELECT secret_encrypted FROM one_siem_config WHERE tenant_id = ?")
                .bind(tenant_id)
                .fetch_optional(&self.pool)
                .await?
                .flatten();
        let secret_encrypted = match secret {
            Some(s) if !s.is_empty() => {
                Some(encrypt_string(s, &self.encryption_key).map_err(|e| PlatformError::Internal(e.to_string()))?)
            }
            _ => existing,
        };
        sqlx::query(
            "INSERT INTO one_siem_config (tenant_id, kind, endpoint, secret_encrypted, enabled, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?) \
             ON CONFLICT(tenant_id) DO UPDATE SET kind = excluded.kind, endpoint = excluded.endpoint, \
                 secret_encrypted = excluded.secret_encrypted, enabled = excluded.enabled, \
                 updated_at = excluded.updated_at",
        )
        .bind(tenant_id)
        .bind(kind)
        .bind(endpoint)
        .bind(&secret_encrypted)
        .bind(enabled)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        self.get_siem_config(tenant_id).await
    }

    /// Decrypted SIEM token, for a real `SiemExporter` to consume.
    pub async fn siem_secret(&self, tenant_id: &str) -> Result<Option<String>, PlatformError> {
        let encrypted: Option<String> =
            sqlx::query_scalar("SELECT secret_encrypted FROM one_siem_config WHERE tenant_id = ?")
                .bind(tenant_id)
                .fetch_optional(&self.pool)
                .await?
                .flatten();
        Ok(encrypted.and_then(|e| decrypt_string(&e, &self.encryption_key).ok()))
    }

    /// Probe the SIEM endpoint (`NoopSiemExporter` by default).
    pub async fn probe_siem(&self, tenant_id: &str) -> Result<SiemStatus, PlatformError> {
        let cfg = self.get_siem_config(tenant_id).await?;
        let secret = self.siem_secret(tenant_id).await?;
        Ok(self
            .siem_exporter
            .probe(SiemSettings {
                kind: cfg.kind.as_deref(),
                endpoint: cfg.endpoint.as_deref(),
                secret: secret.as_deref(),
            })
            .await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn setup() -> (aionui_db::Database, PlatformService) {
        let db = aionui_db::init_database_memory().await.unwrap();
        crate::migrate::run_one_platform_migrations(db.pool()).await.unwrap();
        let service = PlatformService::new(db.pool().clone(), [7u8; 32]);
        (db, service)
    }

    /// Personal edition (no one_user_org table/row): resolve → None, admin gate
    /// rejects with NotInEnterprise. The red line — platform config is gated
    /// off entirely without an enterprise.
    #[tokio::test]
    async fn personal_edition_has_no_membership_and_is_gated() {
        let (_db, service) = setup().await;
        assert!(service.resolve_actor("nobody").await.unwrap().is_none());
        assert_eq!(
            service.require_admin("nobody").await.unwrap_err().code(),
            "NOT_IN_ENTERPRISE"
        );
    }

    async fn seed_membership(pool: &SqlitePool, user_id: &str, tenant_id: &str, role: &str) {
        sqlx::raw_sql(
            "CREATE TABLE IF NOT EXISTS one_user_org (user_id TEXT NOT NULL, tenant_id TEXT NOT NULL, \
                 role TEXT NOT NULL DEFAULT 'member', created_at INTEGER NOT NULL DEFAULT 0, \
                 updated_at INTEGER NOT NULL DEFAULT 0, PRIMARY KEY (user_id, tenant_id));\
             CREATE TABLE IF NOT EXISTS one_active_tenant (user_id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL, \
                 updated_at INTEGER NOT NULL DEFAULT 0);",
        )
        .execute(pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO one_user_org (user_id, tenant_id, role) VALUES (?, ?, ?)")
            .bind(user_id)
            .bind(tenant_id)
            .bind(role)
            .execute(pool)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn admin_gate_accepts_admin_rejects_member() {
        let (db, service) = setup().await;
        seed_membership(db.pool(), "admin1", "t1", "org_admin").await;
        seed_membership(db.pool(), "member1", "t1", "member").await;
        assert_eq!(service.require_admin("admin1").await.unwrap().tenant_id, "t1");
        assert_eq!(service.require_admin("member1").await.unwrap_err().code(), "FORBIDDEN");
    }

    #[tokio::test]
    async fn container_config_roundtrips_redacts_and_stubs_probe() {
        let (_db, service) = setup().await;
        // Absent by default.
        let cfg = service.get_container_config("t1").await.unwrap();
        assert!(!cfg.enabled && !cfg.has_registry_secret);

        let saved = service
            .set_container_config(
                "t1",
                Some("docker"),
                Some("unix:///var/run/docker.sock"),
                Some("ghcr.io/acme/agent:latest"),
                Some("ghcr.io"),
                Some("reg_secret"),
                true,
            )
            .await
            .unwrap();
        assert_eq!(saved.runtime_kind.as_deref(), Some("docker"));
        assert!(saved.has_registry_secret);
        // The DTO never carries the plaintext/ciphertext secret.
        assert!(!serde_json::to_string(&saved).unwrap().contains("reg_secret"));

        // Omitting the secret on a later save keeps the stored one.
        let updated = service
            .set_container_config("t1", Some("docker"), None, None, Some("ghcr.io"), None, false)
            .await
            .unwrap();
        assert!(updated.has_registry_secret && !updated.enabled);
        assert_eq!(
            service.container_registry_secret("t1").await.unwrap().as_deref(),
            Some("reg_secret")
        );

        // Default Noop runtime reports "not configured".
        assert_eq!(service.probe_container("t1").await.unwrap().status, "not_configured");
    }

    #[tokio::test]
    async fn collaboration_config_roundtrips_redacts_and_stubs_probe() {
        let (_db, service) = setup().await;
        let cfg = service.get_collaboration_config("t1").await.unwrap();
        assert!(!cfg.enabled && !cfg.has_secret && !cfg.presence);

        let saved = service
            .set_collaboration_config(
                "t1",
                Some("external"),
                Some("wss://collab.acme.com"),
                Some("relay_tok"),
                true,
                true,
            )
            .await
            .unwrap();
        assert_eq!(saved.provider.as_deref(), Some("external"));
        assert!(saved.has_secret && saved.presence);
        assert!(!serde_json::to_string(&saved).unwrap().contains("relay_tok"));

        let updated = service
            .set_collaboration_config(
                "t1",
                Some("external"),
                Some("wss://collab.acme.com"),
                None,
                false,
                false,
            )
            .await
            .unwrap();
        assert!(updated.has_secret && !updated.presence && !updated.enabled);
        assert_eq!(
            service.collaboration_secret("t1").await.unwrap().as_deref(),
            Some("relay_tok")
        );

        assert_eq!(
            service.probe_collaboration("t1").await.unwrap().status,
            "not_configured"
        );
    }

    #[tokio::test]
    async fn ip_allowlist_roundtrips_and_enforces_when_enabled() {
        let (_db, service) = setup().await;
        // Disabled by default → everyone allowed.
        assert!(service.is_ip_allowed("t1", "8.8.8.8").await.unwrap());

        let saved = service
            .set_ip_allowlist("t1", &["10.0.0.0/8".to_owned(), "192.168.1.5".to_owned()], true)
            .await
            .unwrap();
        assert_eq!(saved.cidrs.len(), 2);
        assert!(saved.enabled);

        // Enabled → only matching IPs allowed.
        assert!(service.is_ip_allowed("t1", "10.3.4.5").await.unwrap());
        assert!(service.is_ip_allowed("t1", "192.168.1.5").await.unwrap());
        assert!(!service.is_ip_allowed("t1", "8.8.8.8").await.unwrap());

        // Toggling off re-allows everyone (no blocking).
        service
            .set_ip_allowlist("t1", &["10.0.0.0/8".to_owned()], false)
            .await
            .unwrap();
        assert!(service.is_ip_allowed("t1", "8.8.8.8").await.unwrap());
    }

    #[tokio::test]
    async fn siem_config_roundtrips_redacts_and_stubs_probe() {
        let (_db, service) = setup().await;
        let cfg = service.get_siem_config("t1").await.unwrap();
        assert!(!cfg.enabled && !cfg.has_secret);

        let saved = service
            .set_siem_config(
                "t1",
                Some("splunk"),
                Some("https://splunk.acme.com:8088"),
                Some("hec_token"),
                true,
            )
            .await
            .unwrap();
        assert_eq!(saved.kind.as_deref(), Some("splunk"));
        assert!(saved.has_secret);
        assert!(!serde_json::to_string(&saved).unwrap().contains("hec_token"));

        // Omitting the token on a later save keeps the stored one.
        let updated = service
            .set_siem_config("t1", Some("splunk"), Some("https://splunk.acme.com:8088"), None, false)
            .await
            .unwrap();
        assert!(updated.has_secret && !updated.enabled);
        assert_eq!(service.siem_secret("t1").await.unwrap().as_deref(), Some("hec_token"));

        assert_eq!(service.probe_siem("t1").await.unwrap().status, "not_configured");
    }
}
