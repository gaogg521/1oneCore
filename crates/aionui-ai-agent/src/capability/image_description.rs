//! Turn a local image into text via a vision-capable delegate model.
//!
//! Mirrors aionrs's `ReadImageTool::describe` / `ViewImageTool::load_image`
//! (`aion-tools/src/{read_image,view_image}.rs`) rather than sharing code
//! with them: 1oneCore already depends on `aion-providers`/`aion-types`
//! directly (`aion-agent` is embedded as a library, not a subprocess), so
//! this stays a same-repo, single-PR change instead of requiring an
//! aionrs-local push + `Cargo.lock` bump before it can build. The two
//! implementations are intentionally not the same crate — they live in
//! separate Cargo workspaces (this repo vs. aionrs), so this is not a
//! violation of the "no duplicate code across crates" rule, which is scoped
//! to crates within one workspace.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use aion_config::compact::CompactConfig;
use aion_config::config::{Config, McpConfig, SessionConfig, ToolsConfig, VisionModelConfig};
use aion_config::file_cache::FileCacheConfig;
use aion_config::hooks::HooksConfig;
use aion_config::logging::LoggingConfig;
use aion_config::plan::PlanConfig;
use aion_config::shell::ShellConfig;
use aion_providers::{LlmProvider, create_provider};
use aion_types::llm::{LlmEvent, LlmRequest};
use aion_types::message::{ContentBlock, ImageUrl, Message, Role, TokenUsage, extension_to_image_media_type};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;

/// Output cap for one description. Vision answers are prose, not file dumps.
const VISION_MAX_TOKENS: u32 = 4_000;

/// Same contract aionrs's `ReadImageTool` uses: describe only what is
/// actually visible, and say so explicitly when part of the image can't be
/// read. A model that hedges less than this tends to fill gaps with guesses.
const VISION_SYSTEM_PROMPT: &str = "You are an image analysis service. You receive one image and a request, and you \
     answer with plain text only. Describe exactly what is present: layout, objects, people, colors, chart series and \
     axis labels, UI elements, and — verbatim — every piece of text you can read, preserving its original language. \
     Never speculate about content you cannot actually see; if part of the image is unreadable, say which part.";

const MAX_IMAGE_SIZE_BYTES: u64 = 20 * 1024 * 1024;
/// A delegate is a best-effort pre-send enrichment, never a reason to leave
/// the user's primary message waiting indefinitely when a gateway stalls.
const VISION_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

fn detect_image_media_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

async fn load_image_content_block(image_path: &str) -> Result<ContentBlock, String> {
    let path = Path::new(image_path);
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .ok_or_else(|| "Image path must have a supported extension".to_owned())?;
    let mime_type =
        extension_to_image_media_type(extension).ok_or_else(|| format!("Unsupported image extension: {extension}"))?;

    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|error| format!("Failed to read image metadata: {error}"))?;
    if !metadata.is_file() {
        return Err("Image path is not a regular file".to_owned());
    }
    if metadata.len() > MAX_IMAGE_SIZE_BYTES {
        return Err(format!("Image exceeds the {MAX_IMAGE_SIZE_BYTES} byte size limit"));
    }

    let bytes = tokio::fs::read(path)
        .await
        .map_err(|error| format!("Failed to read image: {error}"))?;
    if bytes.len() as u64 > MAX_IMAGE_SIZE_BYTES {
        return Err(format!("Image exceeds the {MAX_IMAGE_SIZE_BYTES} byte size limit"));
    }
    let detected_mime_type = detect_image_media_type(&bytes)
        .ok_or_else(|| "File content is not a supported JPEG, PNG, GIF, or WebP image".to_owned())?;
    if detected_mime_type != mime_type {
        return Err(format!(
            "Image content type {detected_mime_type} does not match extension type {mime_type}"
        ));
    }

    let image_url = ImageUrl {
        url: format!("data:{detected_mime_type};base64,{}", STANDARD.encode(bytes)),
    };
    image_url
        .validate()
        .map_err(|error| format!("Failed to prepare image input: {error}"))?;
    Ok(ContentBlock::Image { image_url })
}

/// Build a minimal `aion_config::config::Config` around a resolved vision
/// delegate, populating only what `aion_providers::create_provider` reads
/// (`provider`, `api_key`, `base_url`, `compat`, and — for Bedrock/Vertex —
/// `bedrock`/`vertex`, which `VisionModelConfig` does not carry and so are
/// left `None`, matching the fallback aionrs's own `Config::vision_model_config`
/// takes). Every other field is session/CLI machinery this one-shot call
/// never touches.
fn provider_config_from_delegate(vision: &VisionModelConfig) -> Config {
    Config {
        provider_label: vision.provider_label.clone(),
        provider: vision.provider,
        api_key: vision.api_key.clone(),
        base_url: vision.base_url.clone(),
        model: vision.model.clone(),
        max_tokens: None,
        max_turns: None,
        max_tool_call_malformed_turns: None,
        max_tool_call_failure_turns: None,
        system_prompt: None,
        thinking: None,
        prompt_caching: false,
        compat: vision.compat.clone(),
        tools: ToolsConfig::default(),
        session: SessionConfig::default(),
        compact: CompactConfig::default(),
        plan: PlanConfig::default(),
        shell: ShellConfig::default(),
        file_cache: FileCacheConfig::default(),
        hooks: HooksConfig::default(),
        bedrock: None,
        vertex: None,
        mcp: McpConfig::default(),
        logging: LoggingConfig::default(),
        vision: None,
    }
}

/// Describe a local image via a separately configured vision-capable model.
///
/// Thin wrapper around [`describe_with_provider`] that does the one thing
/// this function alone is responsible for: turning a [`VisionModelConfig`]
/// into a real, network-backed [`LlmProvider`]. Kept separate so tests can
/// exercise the actual describe/anti-hallucination logic against a scripted
/// provider without touching the network.
pub(crate) async fn describe_image_via_delegate(
    vision: &VisionModelConfig,
    image_path: &str,
    instruction: &str,
) -> Result<(String, TokenUsage), String> {
    let provider: Arc<dyn LlmProvider> = create_provider(&provider_config_from_delegate(vision));
    describe_with_provider(provider.as_ref(), &vision.model, image_path, instruction).await
}

/// Returns `Err` rather than an empty success on any failure — including an
/// empty model response — because an empty-but-successful result is exactly
/// what invites a caller to fabricate a description of an image it never
/// actually saw (the same guard `ReadImageTool` enforces).
async fn describe_with_provider(
    provider: &dyn LlmProvider,
    model: &str,
    image_path: &str,
    instruction: &str,
) -> Result<(String, TokenUsage), String> {
    describe_with_provider_with_timeout(provider, model, image_path, instruction, VISION_REQUEST_TIMEOUT).await
}

async fn describe_with_provider_with_timeout(
    provider: &dyn LlmProvider,
    model: &str,
    image_path: &str,
    instruction: &str,
    timeout: Duration,
) -> Result<(String, TokenUsage), String> {
    tokio::time::timeout(
        timeout,
        describe_with_provider_inner(provider, model, image_path, instruction),
    )
    .await
    .map_err(|_| {
        format!(
            "Vision model '{model}' did not finish within {} seconds; the image was not read.",
            timeout.as_secs()
        )
    })?
}

async fn describe_with_provider_inner(
    provider: &dyn LlmProvider,
    model: &str,
    image_path: &str,
    instruction: &str,
) -> Result<(String, TokenUsage), String> {
    let image = load_image_content_block(image_path).await?;

    let request = LlmRequest {
        model: model.to_owned(),
        system: VISION_SYSTEM_PROMPT.to_owned(),
        messages: vec![Message::now(
            Role::User,
            vec![
                image,
                ContentBlock::Text {
                    text: instruction.to_owned(),
                },
            ],
        )],
        tools: Vec::new(),
        max_tokens: Some(VISION_MAX_TOKENS),
        thinking: None,
        reasoning_effort: None,
    };

    let mut stream = provider
        .stream(&request)
        .await
        .map_err(|error| format!("Vision model '{model}' could not be reached: {error}"))?;

    let mut description = String::new();
    let mut usage = TokenUsage::default();
    while let Some(event) = stream.recv().await {
        match event {
            LlmEvent::TextDelta(delta) => description.push_str(&delta),
            LlmEvent::Error(error) => {
                return Err(format!("Vision model '{model}' returned an error: {error}"));
            }
            LlmEvent::Done { usage: done_usage, .. } => {
                usage = done_usage;
                break;
            }
            _ => {}
        }
    }

    let description = description.trim().to_owned();
    if description.is_empty() {
        return Err(format!(
            "Vision model '{model}' returned an empty description. The image was not read; do not guess its contents."
        ));
    }
    Ok((description, usage))
}

#[cfg(test)]
#[path = "image_description_test.rs"]
mod image_description_test;
