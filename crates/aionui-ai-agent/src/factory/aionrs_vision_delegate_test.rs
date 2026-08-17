//! Selection of the vision delegate that `ReadImage` uses for text-only models.

use std::sync::Arc;

use aionui_common::encrypt_string;
use aionui_db::{CreateProviderParams, IProviderRepository, SqliteProviderRepository, init_database_memory};

use super::resolve_vision_delegate;

const TEST_USER_ID: &str = "user-1";
const CONVERSATION_ID: &str = "conv-1";

fn encryption_key() -> [u8; 32] {
    [0x5Au8; 32]
}

struct ProviderFixture {
    id: &'static str,
    platform: &'static str,
    base_url: &'static str,
    models: &'static str,
    enabled: bool,
    model_enabled: Option<&'static str>,
    model_settings: &'static str,
}

impl ProviderFixture {
    fn new(id: &'static str, base_url: &'static str, models: &'static str) -> Self {
        Self {
            id,
            platform: "openai",
            base_url,
            models,
            enabled: true,
            model_enabled: None,
            model_settings: "{}",
        }
    }
}

async fn repo_with(fixtures: Vec<ProviderFixture>) -> Arc<dyn IProviderRepository> {
    let db = init_database_memory().await.expect("in-memory db");
    let repo: Arc<dyn IProviderRepository> = Arc::new(SqliteProviderRepository::new(db.pool().clone()));
    let encrypted = encrypt_string("sk-test", &encryption_key()).expect("encrypt");
    for fixture in fixtures {
        repo.create(CreateProviderParams {
            id: Some(fixture.id),
            user_id: TEST_USER_ID,
            platform: fixture.platform,
            name: fixture.id,
            base_url: fixture.base_url,
            api_key_encrypted: &encrypted,
            models: fixture.models,
            enabled: fixture.enabled,
            capabilities: "[]",
            context_limit: None,
            model_protocols: None,
            model_enabled: fixture.model_enabled,
            model_health: None,
            model_settings: fixture.model_settings,
            bedrock_config: None,
            is_full_url: false,
            managed_by: None,
        })
        .await
        .expect("insert provider");
        // `list` orders by creation time; SQLite millisecond timestamps would
        // otherwise tie and make ordering assertions meaningless.
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    // Leak the in-memory pool for the duration of the test so the database is
    // not dropped while the repository is still in use.
    std::mem::forget(db);
    repo
}

async fn delegate(repo: &dyn IProviderRepository) -> Option<aion_config::config::VisionModelConfig> {
    resolve_vision_delegate(repo, &encryption_key(), TEST_USER_ID, CONVERSATION_ID).await
}

#[tokio::test]
async fn picks_a_catalog_recognized_vision_model() {
    let repo = repo_with(vec![ProviderFixture::new(
        "openai-official",
        "https://api.openai.com/v1",
        r#"["gpt-4o"]"#,
    )])
    .await;

    let chosen = delegate(repo.as_ref()).await.expect("a vision model is available");

    assert_eq!(chosen.model, "gpt-4o");
    assert_eq!(chosen.api_key, "sk-test");
}

/// The exact model the bug report was filed against. It looks like a
/// multimodal id but is text-only, and `image_input.rs` deliberately keeps it
/// off the allowlist — the delegate search must not become a way around that.
#[tokio::test]
async fn refuses_text_only_lookalikes_on_a_custom_gateway() {
    let repo = repo_with(vec![ProviderFixture::new(
        "gateway",
        "https://litellm-internal.123u.com/v1",
        r#"["deepseek-v4-flash","minimax-2-7"]"#,
    )])
    .await;

    assert!(delegate(repo.as_ref()).await.is_none());
}

/// A private gateway serving a genuinely multimodal model is opted in by the
/// user through the per-model setting, not by loosening the allowlist.
#[tokio::test]
async fn honours_an_explicit_per_model_image_input_override() {
    let mut fixture = ProviderFixture::new(
        "gateway",
        "https://litellm-internal.123u.com/v1",
        r#"["house-vision-1"]"#,
    );
    fixture.model_settings = r#"{"house-vision-1":{"image_input":"supported"}}"#;
    let repo = repo_with(vec![fixture]).await;

    let chosen = delegate(repo.as_ref()).await.expect("override honoured");

    assert_eq!(chosen.model, "house-vision-1");
}

#[tokio::test]
async fn an_explicit_unsupported_override_disqualifies_an_allowlisted_model() {
    let mut fixture = ProviderFixture::new("openai-official", "https://api.openai.com/v1", r#"["gpt-4o"]"#);
    fixture.model_settings = r#"{"gpt-4o":{"image_input":"unsupported"}}"#;
    let repo = repo_with(vec![fixture]).await;

    assert!(delegate(repo.as_ref()).await.is_none());
}

#[tokio::test]
async fn skips_disabled_providers_and_disabled_models() {
    let mut disabled_provider = ProviderFixture::new("disabled-provider", "https://api.openai.com/v1", r#"["gpt-4o"]"#);
    disabled_provider.enabled = false;
    let mut disabled_model = ProviderFixture::new("disabled-model", "https://api.openai.com/v1", r#"["gpt-4o"]"#);
    disabled_model.model_enabled = Some(r#"{"gpt-4o":false}"#);
    let repo = repo_with(vec![disabled_provider, disabled_model]).await;

    assert!(delegate(repo.as_ref()).await.is_none());
}

/// A text-only provider earlier in the list must not stop the search.
#[tokio::test]
async fn keeps_looking_past_text_only_providers() {
    let repo = repo_with(vec![
        ProviderFixture::new(
            "gateway",
            "https://litellm-internal.123u.com/v1",
            r#"["deepseek-v4-flash"]"#,
        ),
        ProviderFixture::new("openai-official", "https://api.openai.com/v1", r#"["gpt-4o"]"#),
    ])
    .await;

    let chosen = delegate(repo.as_ref()).await.expect("later provider is reached");

    assert_eq!(chosen.model, "gpt-4o");
}

#[tokio::test]
async fn reports_no_delegate_when_the_user_configured_nothing() {
    let repo = repo_with(Vec::new()).await;

    assert!(delegate(repo.as_ref()).await.is_none());
}
