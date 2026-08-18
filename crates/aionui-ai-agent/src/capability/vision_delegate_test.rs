//! Selection of the vision delegate that `ReadImage` (aionrs) and the ACP
//! image-attachment hook (Claude/Codex bridge) both use for models that
//! cannot see images.

use std::sync::Arc;

use aionui_common::encrypt_string;
use aionui_db::{CreateProviderParams, IProviderRepository, SqliteProviderRepository, init_database_memory};

use std::sync::atomic::{AtomicUsize, Ordering};

use super::{VisionDelegate, resolve_vision_delegate};
use crate::model_policy::ModelAllowlistGate;

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
    resolve_vision_delegate(repo, &encryption_key(), TEST_USER_ID, CONVERSATION_ID, None)
        .await
        .config
}

/// Stands in for `BillingModelAllowlistGate`. Counts its calls so a test can
/// prove the gate was actually consulted — an allowlist assertion that passes
/// because the gate was never reached is not an assertion about the allowlist.
struct StubGate {
    allowed: Vec<&'static str>,
    fails: bool,
    calls: AtomicUsize,
}

impl StubGate {
    fn allowing(allowed: Vec<&'static str>) -> Self {
        Self {
            allowed,
            fails: false,
            calls: AtomicUsize::new(0),
        }
    }

    fn failing() -> Self {
        Self {
            allowed: Vec::new(),
            fails: true,
            calls: AtomicUsize::new(0),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl ModelAllowlistGate for StubGate {
    async fn is_model_allowed(&self, user_id: &str, model: &str) -> Result<bool, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(user_id, TEST_USER_ID, "the gate must be asked about the session's user");
        if self.fails {
            return Err("policy backend unreachable".to_owned());
        }
        Ok(self.allowed.contains(&model))
    }
}

async fn gated_delegate(repo: &dyn IProviderRepository, gate: &StubGate) -> VisionDelegate {
    resolve_vision_delegate(repo, &encryption_key(), TEST_USER_ID, CONVERSATION_ID, Some(gate)).await
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

/// The governance assertion: a model the admin removed from the company's
/// allowlist must not come back in as a vision delegate. Before this gate
/// existed, `resolve_vision_delegate` was a second, ungoverned model choice —
/// the send-path gates (`check_send` / `check_model`) only ever see the
/// *session* model.
#[tokio::test]
async fn refuses_a_vision_model_the_company_allowlist_excludes() {
    let repo = repo_with(vec![ProviderFixture::new(
        "openai-official",
        "https://api.openai.com/v1",
        r#"["gpt-4o"]"#,
    )])
    .await;
    // Allowlist contains only the session model, not the vision candidate.
    let gate = StubGate::allowing(vec!["deepseek-v4-flash"]);

    let resolved = gated_delegate(repo.as_ref(), &gate).await;

    assert!(
        resolved.config.is_none(),
        "an off-allowlist model must not be delegated to"
    );
    assert_eq!(gate.calls(), 1, "the gate must actually have been consulted");
    assert_eq!(resolved.policy_blocked, vec!["gpt-4o".to_owned()]);
    let reason = resolved.unavailable_reason().expect("policy refusal is explained");
    assert!(
        reason.contains("gpt-4o") && reason.contains("administrator"),
        "the message must name the model and point at the admin, not tell the user to add a model: {reason}"
    );
}

#[tokio::test]
async fn allows_a_vision_model_the_company_allowlist_includes() {
    let repo = repo_with(vec![ProviderFixture::new(
        "openai-official",
        "https://api.openai.com/v1",
        r#"["gpt-4o"]"#,
    )])
    .await;
    let gate = StubGate::allowing(vec!["deepseek-v4-flash", "gpt-4o"]);

    let resolved = gated_delegate(repo.as_ref(), &gate).await;

    assert_eq!(
        resolved.config.expect("allowlisted model is delegated to").model,
        "gpt-4o"
    );
    assert_eq!(gate.calls(), 1);
    assert!(resolved.policy_blocked.is_empty());
}

/// A banned candidate must not stop the search: the next allowed one still wins.
#[tokio::test]
async fn keeps_looking_past_a_policy_blocked_candidate() {
    // The second candidate is opted in by the user's own per-model setting, the
    // same way `honours_an_explicit_per_model_image_input_override` does it —
    // independent of what the built-in capability catalog happens to list.
    let mut house = ProviderFixture::new(
        "gateway",
        "https://litellm-internal.123u.com/v1",
        r#"["house-vision-1"]"#,
    );
    house.model_settings = r#"{"house-vision-1":{"image_input":"supported"}}"#;
    let repo = repo_with(vec![
        ProviderFixture::new("openai-official", "https://api.openai.com/v1", r#"["gpt-4o"]"#),
        house,
    ])
    .await;
    let gate = StubGate::allowing(vec!["house-vision-1"]);

    let resolved = gated_delegate(repo.as_ref(), &gate).await;

    assert_eq!(
        resolved.config.as_ref().expect("later allowed model is reached").model,
        "house-vision-1"
    );
    assert_eq!(gate.calls(), 2);
    assert_eq!(resolved.policy_blocked, vec!["gpt-4o".to_owned()]);
    assert!(
        resolved.unavailable_reason().is_none(),
        "a delegate was found; nothing to explain"
    );
}

/// Fail closed. A policy that cannot be evaluated is not a policy that passed —
/// same posture as `BillingSendGate`'s `POLICY_CHECK_FAILED`.
#[tokio::test]
async fn fails_closed_when_the_policy_check_itself_errors() {
    let repo = repo_with(vec![ProviderFixture::new(
        "openai-official",
        "https://api.openai.com/v1",
        r#"["gpt-4o"]"#,
    )])
    .await;
    let gate = StubGate::failing();

    let resolved = gated_delegate(repo.as_ref(), &gate).await;

    assert!(
        resolved.config.is_none(),
        "an unevaluable policy must not be treated as a pass"
    );
    assert_eq!(gate.calls(), 1);
}

/// Personal builds wire no gate at all; behaviour must be exactly as before.
#[tokio::test]
async fn without_a_gate_the_delegate_is_chosen_on_capability_alone() {
    let repo = repo_with(vec![ProviderFixture::new(
        "openai-official",
        "https://api.openai.com/v1",
        r#"["gpt-4o"]"#,
    )])
    .await;

    assert_eq!(delegate(repo.as_ref()).await.expect("personal build").model, "gpt-4o");
}

/// The capability filter runs first, so the gate is never asked about models
/// that were never candidates. This keeps the policy log free of noise about
/// every text-only model the user happens to have configured.
#[tokio::test]
async fn does_not_consult_the_gate_for_models_that_cannot_see_images() {
    let repo = repo_with(vec![ProviderFixture::new(
        "gateway",
        "https://litellm-internal.123u.com/v1",
        r#"["deepseek-v4-flash","minimax-2-7"]"#,
    )])
    .await;
    let gate = StubGate::allowing(vec!["deepseek-v4-flash", "minimax-2-7"]);

    let resolved = gated_delegate(repo.as_ref(), &gate).await;

    assert!(resolved.config.is_none());
    assert_eq!(gate.calls(), 0, "text-only models are filtered before the policy check");
    assert!(
        resolved.unavailable_reason().is_none(),
        "nothing was policy-blocked, so the generic advice is the right message"
    );
}
