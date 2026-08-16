//! Materializing company model channels as local providers.
//!
//! An admin configures a channel once on the company server; this is the other
//! half, on the member's machine. Each visible channel becomes an ordinary row
//! in `providers` — so chat, the ACP bridges and all three media adapter forms
//! pick it up with no branch for where it came from — with two differences:
//!
//! - `base_url` points at the company's model proxy rather than the vendor, and
//! - `api_key` holds the member's revocable **channel token**, never the
//!   company's real credential. That credential stays on the server and is
//!   substituted there. It is the entire point of the design, and the reason
//!   this module takes a token and has no way to accept a vendor key.
//!
//! `managed_by = 'enterprise'` marks the rows as belonging to the company: the
//! UI renders them read-only, and an authoritative sync reconciles them so a
//! channel the admin deleted disappears here too.
//!
//! Personal installs never call this, so nothing about them changes.

use std::sync::Arc;

use aionui_common::encrypt_string;
use aionui_db::{CreateProviderParams, IProviderRepository, models::Provider};
use serde::{Deserialize, Serialize};

use crate::error::SystemError;

/// Marks a provider row as owned by the company rather than the user.
pub const MANAGED_BY_ENTERPRISE: &str = "enterprise";

/// One channel, as the renderer resolved it: the proxy URL it should be reached
/// through and the token this member was issued for it.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedChannelPayload {
    /// Server-side channel id. The local provider id is derived from it so a
    /// re-sync updates the same row instead of piling up duplicates.
    pub channel_id: String,
    pub name: String,
    #[serde(default = "default_platform")]
    pub platform: String,
    /// Already the proxy endpoint — the renderer knows the company server's
    /// address, this process does not.
    pub base_url: String,
    /// The member's channel token. Encrypted at rest here like any other key.
    pub token: String,
    #[serde(default)]
    pub models: Vec<String>,
    /// Per-model settings (`model_kind` / `media_endpoint` / unit price), same
    /// shape as a normal provider's — this is what lets a company channel carry
    /// an image or video model and have the client route it correctly.
    #[serde(default)]
    pub model_settings: Option<serde_json::Value>,
}

fn default_platform() -> String {
    "openai".to_owned()
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedChannelSyncReport {
    pub written: Vec<String>,
    pub removed: Vec<String>,
    /// Channels skipped because a provider of the same id exists and is *not*
    /// managed — i.e. the member made it themselves. Never clobbered.
    pub conflicts: Vec<String>,
}

/// Deterministic local id for a channel, so sync is an upsert.
fn provider_id_for(channel_id: &str) -> String {
    format!("prov_chan_{channel_id}")
}

pub struct ManagedProviderSync {
    repo: Arc<dyn IProviderRepository>,
    encryption_key: [u8; 32],
}

impl ManagedProviderSync {
    pub fn new(repo: Arc<dyn IProviderRepository>, encryption_key: [u8; 32]) -> Self {
        Self { repo, encryption_key }
    }

    /// Reconcile the member's enterprise-managed providers against `channels`.
    ///
    /// `authoritative` means "this is the complete set the member can see", so
    /// managed rows not present are removed. The renderer only passes `true`
    /// when its fetch succeeded — a server it could not reach must leave the
    /// local rows alone rather than wipe the member's working setup, the same
    /// offline-first contract the team skill and MCP syncs use.
    ///
    /// Personal rows are never touched under any circumstances.
    pub async fn sync(
        &self,
        user_id: &str,
        channels: &[ManagedChannelPayload],
        authoritative: bool,
    ) -> Result<ManagedChannelSyncReport, SystemError> {
        let mut report = ManagedChannelSyncReport::default();
        let existing = self.repo.list(user_id).await?;

        for channel in channels {
            let id = provider_id_for(&channel.channel_id);

            // A personal provider that happens to occupy this id is the
            // member's, not ours. Refusing is the only safe move: overwriting
            // would silently replace a key they entered themselves.
            if let Some(row) = existing.iter().find(|p| p.id == id)
                && row.managed_by.as_deref() != Some(MANAGED_BY_ENTERPRISE)
            {
                report.conflicts.push(channel.name.clone());
                continue;
            }

            self.write_channel(user_id, channel, &id, &existing).await?;
            report.written.push(channel.name.clone());
        }

        if authoritative {
            let wanted: Vec<String> = channels.iter().map(|c| provider_id_for(&c.channel_id)).collect();
            for row in existing
                .iter()
                .filter(|p| p.managed_by.as_deref() == Some(MANAGED_BY_ENTERPRISE))
                .filter(|p| !wanted.contains(&p.id))
            {
                self.repo.delete(user_id, &row.id).await?;
                report.removed.push(row.name.clone());
            }
        }

        Ok(report)
    }

    async fn write_channel(
        &self,
        user_id: &str,
        channel: &ManagedChannelPayload,
        id: &str,
        existing: &[Provider],
    ) -> Result<(), SystemError> {
        let encrypted = encrypt_string(&channel.token, &self.encryption_key)?;
        let models = serde_json::to_string(&channel.models)
            .map_err(|e| SystemError::Internal(format!("failed to serialize channel models: {e}")))?;
        let model_settings = match &channel.model_settings {
            Some(value) => serde_json::to_string(value)
                .map_err(|e| SystemError::Internal(format!("failed to serialize channel model settings: {e}")))?,
            None => "{}".to_owned(),
        };

        // Replace rather than update: the channel definition on the server is
        // the whole truth for these rows, and a partial update would let a
        // stale local field (a model the admin removed, an old proxy address)
        // survive a sync that was supposed to correct it.
        if existing.iter().any(|p| p.id == id) {
            self.repo.delete(user_id, id).await?;
        }

        self.repo
            .create(CreateProviderParams {
                id: Some(id),
                user_id,
                platform: &channel.platform,
                name: &channel.name,
                base_url: &channel.base_url,
                api_key_encrypted: &encrypted,
                models: &models,
                enabled: true,
                capabilities: "[]",
                context_limit: None,
                model_protocols: None,
                model_enabled: None,
                model_health: None,
                model_settings: &model_settings,
                bedrock_config: None,
                is_full_url: false,
                managed_by: Some(MANAGED_BY_ENTERPRISE),
            })
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_db::SqliteProviderRepository;
    use aionui_db::init_database_memory;

    const KEY: [u8; 32] = [3u8; 32];
    const TEST_USER_ID: &str = "system_default_user";

    fn channel(id: &str, name: &str, token: &str) -> ManagedChannelPayload {
        ManagedChannelPayload {
            channel_id: id.to_owned(),
            name: name.to_owned(),
            platform: "openai".to_owned(),
            base_url: format!("https://one.corp.example/api/one/model-proxy/{id}"),
            token: token.to_owned(),
            models: vec!["gpt-image-2".to_owned()],
            model_settings: None,
        }
    }

    async fn sync_service() -> (ManagedProviderSync, Arc<dyn IProviderRepository>, aionui_db::Database) {
        let db = init_database_memory().await.unwrap();
        let repo: Arc<dyn IProviderRepository> = Arc::new(SqliteProviderRepository::new(db.pool().clone()));
        (ManagedProviderSync::new(repo.clone(), KEY), repo, db)
    }

    #[tokio::test]
    async fn a_channel_becomes_a_usable_provider_pointed_at_the_proxy() {
        let (sync, repo, _db) = sync_service().await;
        let report = sync
            .sync(TEST_USER_ID, &[channel("ochan_1", "corp-gateway", "onech-tok")], true)
            .await
            .unwrap();
        assert_eq!(report.written, vec!["corp-gateway".to_owned()]);

        let rows = repo.list(TEST_USER_ID).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].base_url, "https://one.corp.example/api/one/model-proxy/ochan_1");
        assert_eq!(rows[0].managed_by.as_deref(), Some("enterprise"));
        assert!(rows[0].enabled);
    }

    /// What lands on the member's disk is the revocable token, never a vendor
    /// credential — the server holds that and substitutes it at the proxy.
    #[tokio::test]
    async fn what_is_stored_locally_is_the_channel_token() {
        let (sync, repo, _db) = sync_service().await;
        sync.sync(TEST_USER_ID, &[channel("ochan_1", "corp-gateway", "onech-tok")], true)
            .await
            .unwrap();

        let rows = repo.list(TEST_USER_ID).await.unwrap();
        let stored = aionui_common::decrypt_string(&rows[0].api_key_encrypted, &KEY).unwrap();
        assert_eq!(stored, "onech-tok");
    }

    /// Re-syncing must update in place, not accumulate a row per run.
    #[tokio::test]
    async fn re_syncing_updates_the_same_row() {
        let (sync, repo, _db) = sync_service().await;
        sync.sync(TEST_USER_ID, &[channel("ochan_1", "corp-gateway", "tok-1")], true)
            .await
            .unwrap();
        sync.sync(
            TEST_USER_ID,
            &[channel("ochan_1", "corp-gateway-renamed", "tok-2")],
            true,
        )
        .await
        .unwrap();

        let rows = repo.list(TEST_USER_ID).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "corp-gateway-renamed");
        assert_eq!(
            aionui_common::decrypt_string(&rows[0].api_key_encrypted, &KEY).unwrap(),
            "tok-2"
        );
    }

    /// A channel the admin withdrew must stop being usable here too.
    #[tokio::test]
    async fn an_authoritative_sync_removes_channels_that_went_away() {
        let (sync, repo, _db) = sync_service().await;
        sync.sync(
            TEST_USER_ID,
            &[channel("ochan_1", "a", "t1"), channel("ochan_2", "b", "t2")],
            true,
        )
        .await
        .unwrap();

        let report = sync
            .sync(TEST_USER_ID, &[channel("ochan_1", "a", "t1")], true)
            .await
            .unwrap();
        assert_eq!(report.removed, vec!["b".to_owned()]);
        assert_eq!(repo.list(TEST_USER_ID).await.unwrap().len(), 1);
    }

    /// Offline-first: a server the client could not reach must not wipe the
    /// member's provisioned channels.
    #[tokio::test]
    async fn a_non_authoritative_sync_never_removes_anything() {
        let (sync, repo, _db) = sync_service().await;
        sync.sync(TEST_USER_ID, &[channel("ochan_1", "a", "t1")], true)
            .await
            .unwrap();

        let report = sync.sync(TEST_USER_ID, &[], false).await.unwrap();
        assert!(report.removed.is_empty());
        assert_eq!(repo.list(TEST_USER_ID).await.unwrap().len(), 1);
    }

    /// The member's own providers are not ours to touch, under any sync.
    #[tokio::test]
    async fn personal_providers_are_never_removed_or_overwritten() {
        let (sync, repo, _db) = sync_service().await;
        repo.create(CreateProviderParams {
            id: Some("prov_chan_ochan_1"), // deliberately the id a channel would claim
            user_id: TEST_USER_ID,
            platform: "openai",
            name: "my own key",
            base_url: "https://api.openai.com",
            api_key_encrypted: "personal",
            models: "[]",
            enabled: true,
            capabilities: "[]",
            context_limit: None,
            model_protocols: None,
            model_enabled: None,
            model_health: None,
            model_settings: "{}",
            bedrock_config: None,
            is_full_url: false,
            managed_by: None,
        })
        .await
        .unwrap();

        let report = sync
            .sync(TEST_USER_ID, &[channel("ochan_1", "corp-gateway", "tok")], true)
            .await
            .unwrap();
        assert_eq!(report.conflicts, vec!["corp-gateway".to_owned()]);
        assert!(report.written.is_empty());

        let rows = repo.list(TEST_USER_ID).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "my own key", "a personal provider was clobbered");
        assert_eq!(rows[0].api_key_encrypted, "personal");
    }

    /// A member cannot edit or delete what the company provisioned. The sync
    /// would restore it anyway, so allowing the write would only produce a
    /// change that silently reverts later.
    #[tokio::test]
    async fn a_managed_provider_cannot_be_edited_or_deleted_locally() {
        use crate::provider::ProviderService;
        use aionui_api_types::UpdateProviderRequest;

        let (sync, repo, _db) = sync_service().await;
        sync.sync(TEST_USER_ID, &[channel("ochan_1", "corp", "tok")], true)
            .await
            .unwrap();
        let service = ProviderService::new(repo.clone(), KEY);

        let edit = service
            .update(
                TEST_USER_ID,
                "prov_chan_ochan_1",
                UpdateProviderRequest {
                    name: Some("mine now".to_owned()),
                    ..Default::default()
                },
            )
            .await;
        assert!(matches!(edit, Err(SystemError::BadRequest(_))), "edit was allowed");

        let remove = service.delete(TEST_USER_ID, "prov_chan_ochan_1").await;
        assert!(matches!(remove, Err(SystemError::BadRequest(_))), "delete was allowed");

        let rows = repo.list(TEST_USER_ID).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "corp");
    }

    /// …while the member's own providers stay fully theirs.
    #[tokio::test]
    async fn a_personal_provider_is_still_editable() {
        use crate::provider::ProviderService;
        use aionui_api_types::UpdateProviderRequest;

        let (_sync, repo, _db) = sync_service().await;
        repo.create(CreateProviderParams {
            id: Some("prov_mine"),
            user_id: TEST_USER_ID,
            platform: "openai",
            name: "mine",
            base_url: "https://api.openai.com",
            api_key_encrypted: &aionui_common::encrypt_string("sk-mine", &KEY).unwrap(),
            models: "[]",
            enabled: true,
            capabilities: "[]",
            context_limit: None,
            model_protocols: None,
            model_enabled: None,
            model_health: None,
            model_settings: "{}",
            bedrock_config: None,
            is_full_url: false,
            managed_by: None,
        })
        .await
        .unwrap();

        let service = ProviderService::new(repo.clone(), KEY);
        service
            .update(
                TEST_USER_ID,
                "prov_mine",
                UpdateProviderRequest {
                    name: Some("renamed".to_owned()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(repo.list(TEST_USER_ID).await.unwrap()[0].name, "renamed");
    }

    /// An empty authoritative sync is how "the company has no channels for you"
    /// arrives, and it must clear managed rows while leaving personal ones.
    #[tokio::test]
    async fn clearing_all_channels_leaves_personal_providers_alone() {
        let (sync, repo, _db) = sync_service().await;
        repo.create(CreateProviderParams {
            id: Some("prov_personal"),
            user_id: TEST_USER_ID,
            platform: "openai",
            name: "mine",
            base_url: "https://api.openai.com",
            api_key_encrypted: "personal",
            models: "[]",
            enabled: true,
            capabilities: "[]",
            context_limit: None,
            model_protocols: None,
            model_enabled: None,
            model_health: None,
            model_settings: "{}",
            bedrock_config: None,
            is_full_url: false,
            managed_by: None,
        })
        .await
        .unwrap();
        sync.sync(TEST_USER_ID, &[channel("ochan_1", "corp", "tok")], true)
            .await
            .unwrap();

        sync.sync(TEST_USER_ID, &[], true).await.unwrap();

        let rows = repo.list(TEST_USER_ID).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "mine");
    }
}
