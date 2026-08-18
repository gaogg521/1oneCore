use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use tempfile::TempDir;
use tokio::sync::mpsc;

use aion_providers::{LlmProvider, ProviderError};
use aion_types::llm::{LlmEvent, LlmRequest};
use aion_types::message::{StopReason, TokenUsage};

use super::{describe_with_provider, describe_with_provider_with_timeout};

/// A stand-in vision model: records the request it was given and replays a
/// scripted event sequence. Mirrors aionrs's own `ReadImage` test double
/// (`aion-tools/src/read_image_test.rs::ScriptedVisionProvider`) since this
/// module intentionally reimplements (not shares) that crate's logic.
struct ScriptedVisionProvider {
    events: Vec<LlmEvent>,
    requests: Arc<Mutex<Vec<LlmRequest>>>,
}

impl ScriptedVisionProvider {
    fn replying(text: &str) -> Self {
        Self {
            events: vec![
                LlmEvent::TextDelta(text.to_owned()),
                LlmEvent::Done {
                    stop_reason: StopReason::EndTurn,
                    usage: TokenUsage {
                        input_tokens: 120,
                        output_tokens: 40,
                        ..TokenUsage::default()
                    },
                },
            ],
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn with_events(events: Vec<LlmEvent>) -> Self {
        Self {
            events,
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

#[async_trait]
impl LlmProvider for ScriptedVisionProvider {
    async fn stream(&self, request: &LlmRequest) -> Result<mpsc::Receiver<LlmEvent>, ProviderError> {
        self.requests.lock().unwrap().push(request.clone());
        let (tx, rx) = mpsc::channel(16);
        for event in &self.events {
            let _ = tx.send(event.clone()).await;
        }
        Ok(rx)
    }
}

struct UnreachableVisionProvider;

#[async_trait]
impl LlmProvider for UnreachableVisionProvider {
    async fn stream(&self, _request: &LlmRequest) -> Result<mpsc::Receiver<LlmEvent>, ProviderError> {
        Err(ProviderError::Connection("connection refused".to_owned()))
    }
}

struct NeverEndingVisionProvider {
    sender: Mutex<Option<mpsc::Sender<LlmEvent>>>,
}

#[async_trait]
impl LlmProvider for NeverEndingVisionProvider {
    async fn stream(&self, _request: &LlmRequest) -> Result<mpsc::Receiver<LlmEvent>, ProviderError> {
        let (tx, rx) = mpsc::channel(1);
        *self.sender.lock().unwrap() = Some(tx);
        Ok(rx)
    }
}

fn write_png(directory: &TempDir) -> PathBuf {
    let path = directory.path().join("chart.png");
    let png = STANDARD
        .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVQIHWP4z8DwHwAFgAI/ScL3WQAAAABJRU5ErkJggg==")
        .expect("decode PNG fixture");
    std::fs::write(&path, png).expect("write image fixture");
    path
}

#[tokio::test]
async fn returns_the_delegate_text_and_usage_for_a_valid_image() {
    let dir = TempDir::new().expect("tempdir");
    let path = write_png(&dir);
    let provider = ScriptedVisionProvider::replying("A bar chart titled Q4 Revenue.");

    let (description, usage) = describe_with_provider(&provider, "gpt-4o", path.to_str().unwrap(), "Describe this")
        .await
        .expect("delegate call succeeds");

    assert_eq!(description, "A bar chart titled Q4 Revenue.");
    assert_eq!(usage.input_tokens, 120);
    assert_eq!(usage.output_tokens, 40);
}

#[tokio::test]
async fn sends_the_image_and_instruction_to_the_delegate_model() {
    let dir = TempDir::new().expect("tempdir");
    let path = write_png(&dir);
    let provider = ScriptedVisionProvider::replying("ok");

    describe_with_provider(&provider, "gpt-4o", path.to_str().unwrap(), "transcribe all the text")
        .await
        .expect("delegate call succeeds");

    let sent = provider.requests.lock().unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].model, "gpt-4o");
    assert_eq!(sent[0].messages.len(), 1);
    // One image block plus one text block carrying the instruction.
    assert_eq!(sent[0].messages[0].content.len(), 2);
}

/// An empty-but-successful reply is exactly what would let a caller fabricate
/// a description of an image it never actually saw — must be `Err`, never a
/// silent empty success.
#[tokio::test]
async fn treats_an_empty_delegate_reply_as_an_error() {
    let dir = TempDir::new().expect("tempdir");
    let path = write_png(&dir);
    let provider = ScriptedVisionProvider::with_events(vec![LlmEvent::Done {
        stop_reason: StopReason::EndTurn,
        usage: TokenUsage::default(),
    }]);

    let error = describe_with_provider(&provider, "gpt-4o", path.to_str().unwrap(), "Describe this")
        .await
        .expect_err("an empty description must not be treated as success");

    assert!(
        error.contains("empty"),
        "error should say the description was empty: {error}"
    );
    assert!(
        error.contains("do not guess"),
        "error should carry the anti-hallucination instruction: {error}"
    );
}

#[tokio::test]
async fn propagates_a_provider_error_event() {
    let dir = TempDir::new().expect("tempdir");
    let path = write_png(&dir);
    let provider = ScriptedVisionProvider::with_events(vec![LlmEvent::Error("rate limited".to_owned())]);

    let error = describe_with_provider(&provider, "gpt-4o", path.to_str().unwrap(), "Describe this")
        .await
        .expect_err("a mid-stream error must fail the call");

    assert!(error.contains("rate limited"));
}

#[tokio::test]
async fn fails_when_the_delegate_model_is_unreachable() {
    let dir = TempDir::new().expect("tempdir");
    let path = write_png(&dir);

    let error = describe_with_provider(
        &UnreachableVisionProvider,
        "gpt-4o",
        path.to_str().unwrap(),
        "Describe this",
    )
    .await
    .expect_err("connection failure must be surfaced, not swallowed");

    assert!(error.contains("could not be reached"));
}

#[tokio::test]
async fn times_out_a_stalled_delegate_so_the_primary_message_can_continue() {
    let dir = TempDir::new().expect("tempdir");
    let path = write_png(&dir);
    let provider = NeverEndingVisionProvider {
        sender: Mutex::new(None),
    };

    let error = describe_with_provider_with_timeout(
        &provider,
        "gpt-4o",
        path.to_str().unwrap(),
        "Describe this",
        Duration::from_millis(5),
    )
    .await
    .expect_err("a stalled vision delegate must time out");

    assert!(error.contains("did not finish"));
    assert!(error.contains("image was not read"));
}

#[tokio::test]
async fn rejects_an_unsupported_file_extension_before_any_network_call() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("notes.txt");
    std::fs::write(&path, b"hello").expect("write fixture");
    let provider = ScriptedVisionProvider::replying("should never be called");

    let error = describe_with_provider(&provider, "gpt-4o", path.to_str().unwrap(), "Describe this")
        .await
        .expect_err("a non-image extension must be rejected");

    assert!(error.contains("Unsupported image extension"));
    assert!(
        provider.requests.lock().unwrap().is_empty(),
        "the delegate must never be called for a file that was never a valid image"
    );
}

#[tokio::test]
async fn rejects_a_file_whose_content_does_not_match_its_extension() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("fake.png");
    std::fs::write(&path, b"not actually a png").expect("write fixture");
    let provider = ScriptedVisionProvider::replying("should never be called");

    let error = describe_with_provider(&provider, "gpt-4o", path.to_str().unwrap(), "Describe this")
        .await
        .expect_err("magic-byte sniffing must catch a mislabeled file");

    assert!(error.contains("not a supported"));
    assert!(provider.requests.lock().unwrap().is_empty());
}
