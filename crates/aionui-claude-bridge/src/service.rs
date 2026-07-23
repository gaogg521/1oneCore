use std::sync::Arc;

use aionui_db::{ClaudeBridgeConfig, IClaudeBridgeConfigRepository};

use crate::error::ClaudeBridgeError;

pub struct ClaudeBridgeService {
    config_repo: Arc<dyn IClaudeBridgeConfigRepository>,
}

impl ClaudeBridgeService {
    pub fn new(config_repo: Arc<dyn IClaudeBridgeConfigRepository>) -> Self {
        Self { config_repo }
    }

    /// Returns the current bridge config, or `None` if never configured.
    pub async fn get_config(&self) -> Result<Option<ClaudeBridgeConfig>, ClaudeBridgeError> {
        Ok(self.config_repo.get().await?)
    }

    /// Enable/disable the bridge and/or change which saved provider+model
    /// Claude Code is launched against.
    pub async fn upsert_config(
        &self,
        enabled: bool,
        provider_id: Option<&str>,
        model: Option<&str>,
    ) -> Result<ClaudeBridgeConfig, ClaudeBridgeError> {
        Ok(self.config_repo.upsert(enabled, provider_id, model).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_db::{SqliteClaudeBridgeConfigRepository, init_database_memory};

    async fn setup() -> ClaudeBridgeService {
        let db = init_database_memory().await.unwrap();
        let repo: Arc<dyn IClaudeBridgeConfigRepository> =
            Arc::new(SqliteClaudeBridgeConfigRepository::new(db.pool().clone()));
        ClaudeBridgeService::new(repo)
    }

    #[tokio::test]
    async fn get_config_returns_none_when_unconfigured() {
        let service = setup().await;
        assert!(service.get_config().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn upsert_config_then_get_returns_same() {
        let service = setup().await;
        service.upsert_config(true, Some("prov-1"), Some("glm-5-2")).await.unwrap();

        let config = service.get_config().await.unwrap().unwrap();
        assert!(config.enabled);
        assert_eq!(config.provider_id.as_deref(), Some("prov-1"));
        assert_eq!(config.model.as_deref(), Some("glm-5-2"));
    }
}
