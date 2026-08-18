//! Rewrite flattened image attachment paths for text-only bridge targets.
//!
//! ACP has no attachment field in its prompt request. The project layer
//! therefore appends resolved paths under `AIONUI_FILES_MARKER`, and the
//! external Claude/Codex CLI receives those paths as ordinary text. A bridged
//! custom model that cannot accept images must never be left to infer their
//! contents from a filename: this hook turns each *verified* image path into
//! a vision-delegate description, or an explicit unreadable-image notice.

use std::path::Path;
use std::sync::Arc;

use aion_config::config::VisionModelConfig;
use aion_types::message::{TokenUsage, extension_to_image_media_type};
use aionui_common::constants::AIONUI_FILES_MARKER;

use crate::capability::image_description::describe_image_via_delegate;
use crate::capability::local_ocr_skill::{
    host_local_ocr_skill_name, is_bundled_local_ocr_skill, with_host_local_ocr_skill,
};
use crate::capability::prompt_pipeline::{PreSendHook, PromptCtx};
use crate::capability::skill_manager::SkillDefinition;
use crate::capability::vision_delegate::AcpVisionPolicy;
use crate::manager::acp::hooks::emit_hook_warning;
use crate::protocol::events::{AgentStreamEvent, DelegateUsageEventData};

const HOOK_NAME: &str = "bridge_image_vision";
const DESCRIPTION_INSTRUCTION: &str = "Describe this image for the main assistant. Include all visible text and do not guess anything that is unreadable.";

#[async_trait::async_trait]
trait ImageDescriber: Send + Sync {
    async fn describe(&self, vision: &VisionModelConfig, image_path: &str) -> Result<(String, TokenUsage), String>;
}

struct DelegateImageDescriber;

#[async_trait::async_trait]
impl ImageDescriber for DelegateImageDescriber {
    async fn describe(&self, vision: &VisionModelConfig, image_path: &str) -> Result<(String, TokenUsage), String> {
        describe_image_via_delegate(vision, image_path, DESCRIPTION_INSTRUCTION).await
    }
}

/// ACP pre-send hook for image attachments in bridged text-only sessions.
pub struct ImageAttachmentVisionHook {
    describer: Arc<dyn ImageDescriber>,
}

impl Default for ImageAttachmentVisionHook {
    fn default() -> Self {
        Self {
            describer: Arc::new(DelegateImageDescriber),
        }
    }
}

#[async_trait::async_trait]
impl PreSendHook for ImageAttachmentVisionHook {
    async fn pre_send(&self, ctx: &mut PromptCtx<'_>, prompt: String) -> String {
        // A model that natively accepts the attachment stays on its direct
        // multimodal path. Only a bridge policy reaches the local OCR search.
        let local_ocr = if matches!(&ctx.params.vision_policy, AcpVisionPolicy::NotBridged) {
            Ok(None)
        } else {
            find_host_local_ocr_skill(ctx).await
        };
        let (local_ocr, mut setup_warnings) = match local_ocr {
            Ok(skill) => (skill, Vec::new()),
            Err(error) => (None, vec![error]),
        };
        let mut outcome = rewrite_image_attachment_paths(
            prompt,
            ctx.files,
            &ctx.params.vision_policy,
            local_ocr.as_ref(),
            self.describer.as_ref(),
        )
        .await;
        setup_warnings.append(&mut outcome.warnings);

        for warning in setup_warnings {
            emit_hook_warning(ctx, HOOK_NAME, warning);
        }
        for usage in outcome.delegate_usage {
            ctx.runtime.emit(AgentStreamEvent::DelegateUsage(usage));
        }
        outcome.prompt
    }
}

struct RewriteOutcome {
    prompt: String,
    warnings: Vec<String>,
    delegate_usage: Vec<DelegateUsageEventData>,
}

/// A bundled local OCR skill loaded from the materialized, trusted corpus.
/// The script directory is supplied explicitly because ACP text bridges do not
/// receive a native skill mount as an attachment field.
struct LocalOcrSkill {
    name: String,
    script_dir: String,
    instructions: String,
}

async fn find_host_local_ocr_skill(ctx: &PromptCtx<'_>) -> Result<Option<LocalOcrSkill>, String> {
    let Some(expected_name) = host_local_ocr_skill_name() else {
        return Ok(None);
    };
    let selected = with_host_local_ocr_skill(&ctx.params.config.skills);
    let discovered = ctx
        .skill_manager
        .discover_skills_for_user(&ctx.params.user_id, Some(&selected), None)
        .await;
    if !discovered.iter().any(|skill| skill.name == expected_name) {
        return Ok(None);
    }

    let Some(definition) = ctx.skill_manager.get_skill(expected_name).await else {
        return Err(format!(
            "The default local OCR skill '{expected_name}' could not be read."
        ));
    };
    local_ocr_from_definition(definition, expected_name).map(Some)
}

fn local_ocr_from_definition(definition: SkillDefinition, expected_name: &str) -> Result<LocalOcrSkill, String> {
    if definition.name != expected_name || !is_bundled_local_ocr_skill(&definition.name) {
        return Err(format!(
            "The default local OCR skill '{expected_name}' was replaced by an unexpected skill."
        ));
    }
    if definition.source != aionui_extension::SkillSource::Builtin {
        return Err(format!(
            "The default local OCR skill '{expected_name}' is not a bundled skill."
        ));
    }
    let script_dir = definition
        .location
        .parent()
        .ok_or_else(|| format!("The default local OCR skill '{expected_name}' has no skill directory."))?
        .to_string_lossy()
        .into_owned();
    let instructions = definition
        .body
        .filter(|body| !body.trim().is_empty())
        .ok_or_else(|| format!("The default local OCR skill '{expected_name}' has no instructions."))?;
    Ok(LocalOcrSkill {
        name: definition.name,
        script_dir,
        instructions,
    })
}

/// Rewrite only paths that the structured attachment list confirms positionally.
/// This deliberately does not search arbitrary user prose for paths: a user can
/// type the marker or a filename themselves, and changing that text would be a
/// surprising and unsafe prompt mutation.
async fn rewrite_image_attachment_paths(
    prompt: String,
    files: &[String],
    policy: &AcpVisionPolicy,
    local_ocr: Option<&LocalOcrSkill>,
    describer: &dyn ImageDescriber,
) -> RewriteOutcome {
    if matches!(policy, AcpVisionPolicy::NotBridged) || files.is_empty() {
        return RewriteOutcome {
            prompt,
            warnings: Vec::new(),
            delegate_usage: Vec::new(),
        };
    }

    let marker = format!("{AIONUI_FILES_MARKER}\n");
    let Some((prefix, attachment_lines)) = prompt.split_once(&marker) else {
        return RewriteOutcome {
            prompt,
            warnings: vec!["Attachment metadata was present but its flattened prompt block was missing; image paths were left unchanged.".to_owned()],
            delegate_usage: Vec::new(),
        };
    };
    let mut lines: Vec<String> = attachment_lines.split('\n').map(str::to_owned).collect();
    if lines.len() < files.len() {
        return RewriteOutcome {
            prompt,
            warnings: vec!["The flattened attachment block had fewer paths than the structured attachment metadata; image paths were left unchanged.".to_owned()],
            delegate_usage: Vec::new(),
        };
    }

    let mut warnings = Vec::new();
    let mut delegate_usage = Vec::new();
    for (index, image_path) in files.iter().enumerate() {
        if lines[index] != *image_path {
            warnings.push(format!(
                "Attachment path at position {} did not match its structured metadata; it was left unchanged.",
                index + 1
            ));
            continue;
        }
        if !is_supported_image_path(image_path) {
            continue;
        }

        // Local OCR is deliberately first for text-only bridge sessions. Its
        // result is obtained by the agent using the bundled host script; the
        // remote vision delegate remains a bounded fallback only when this
        // local skill is unavailable.
        if let Some(skill) = local_ocr {
            lines[index] = local_ocr_instruction(skill, image_path);
            continue;
        }

        match policy {
            AcpVisionPolicy::NotBridged => unreachable!("handled before parsing attachment paths"),
            AcpVisionPolicy::Unavailable { reason } => {
                lines[index] = unreadable_image_notice(reason.as_deref());
            }
            AcpVisionPolicy::Delegate(vision) => match describer.describe(vision, image_path).await {
                Ok((description, usage)) => {
                    lines[index] = format!("<aionui-image-description>\n{description}\n</aionui-image-description>");
                    delegate_usage.push(DelegateUsageEventData {
                        model: vision.model.clone(),
                        input_tokens: usage.input_tokens as i64,
                        output_tokens: usage.output_tokens as i64,
                    });
                }
                Err(error) => {
                    warnings.push(format!(
                        "The vision delegate could not read attachment '{}': {error}",
                        image_path
                    ));
                    lines[index] = unreadable_image_notice(Some(&error));
                }
            },
        }
    }

    RewriteOutcome {
        prompt: format!("{prefix}{marker}{}", lines.join("\n")),
        warnings,
        delegate_usage,
    }
}

fn local_ocr_instruction(skill: &LocalOcrSkill, image_path: &str) -> String {
    format!(
        "<aionui-local-ocr-skill name=\"{}\" directory=\"{}\">\n{}\n</aionui-local-ocr-skill>\n\n[Required before answering: use the local OCR skill above on this exact attachment path. Do not upload it or install anything. OCR returns text only; if it is unavailable or fails, say so and do not infer the image's contents.]\n<image-path>{image_path}</image-path>",
        skill.name, skill.script_dir, skill.instructions
    )
}

fn is_supported_image_path(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .and_then(extension_to_image_media_type)
        .is_some()
}

fn unreadable_image_notice(reason: Option<&str>) -> String {
    let reason = reason
        .filter(|reason| !reason.trim().is_empty())
        .unwrap_or("No permitted vision-capable model is available to read this attachment.");
    format!(
        "[Image attachment could not be read: {reason} The image contents are unavailable. Do NOT guess, infer, or state what the image shows — including from the file name, path, title, user prompt, surrounding context, or metadata. Reply only that the image could not be read and its contents are unavailable.]"
    )
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use aion_config::compat::ProviderCompat;
    use aion_config::config::ProviderType;

    use super::*;

    struct ScriptedDescriber {
        responses: Mutex<VecDeque<Result<(String, TokenUsage), String>>>,
    }

    #[async_trait::async_trait]
    impl ImageDescriber for ScriptedDescriber {
        async fn describe(
            &self,
            _vision: &VisionModelConfig,
            _image_path: &str,
        ) -> Result<(String, TokenUsage), String> {
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("test configured a response for every image")
        }
    }

    fn vision_policy() -> AcpVisionPolicy {
        AcpVisionPolicy::Delegate(Box::new(VisionModelConfig {
            provider_label: "Vision test".into(),
            provider: ProviderType::OpenAI,
            api_key: "test-key".into(),
            base_url: "https://example.test/v1".into(),
            model: "vision-test".into(),
            compat: ProviderCompat::default(),
        }))
    }

    fn attachment_prompt(lines: &[&str]) -> String {
        format!(
            "Explain the attachments.\n\n{AIONUI_FILES_MARKER}\n{}",
            lines.join("\n")
        )
    }

    #[tokio::test]
    async fn not_bridged_is_byte_identical() {
        let prompt = attachment_prompt(&[r"C:\\temp\\photo.png"]);
        let files = vec![r"C:\\temp\\photo.png".to_owned()];
        let describer = ScriptedDescriber {
            responses: Mutex::new(VecDeque::new()),
        };

        let outcome =
            rewrite_image_attachment_paths(prompt.clone(), &files, &AcpVisionPolicy::NotBridged, None, &describer)
                .await;

        assert_eq!(outcome.prompt, prompt);
        assert!(outcome.warnings.is_empty());
        assert!(outcome.delegate_usage.is_empty());
    }

    #[tokio::test]
    async fn unavailable_rewrites_only_verified_image_paths() {
        let prompt = attachment_prompt(&[r"C:\\temp\\photo.png", r"C:\\temp\\notes.pdf"]);
        let files = vec![r"C:\\temp\\photo.png".to_owned(), r"C:\\temp\\notes.pdf".to_owned()];
        let describer = ScriptedDescriber {
            responses: Mutex::new(VecDeque::new()),
        };

        let outcome = rewrite_image_attachment_paths(
            prompt,
            &files,
            &AcpVisionPolicy::Unavailable {
                reason: Some("The organization policy blocks all vision delegates.".into()),
            },
            None,
            &describer,
        )
        .await;

        assert!(outcome.prompt.contains("organization policy blocks"));
        assert!(outcome.prompt.contains("Do NOT guess"));
        assert!(outcome.prompt.contains("file name, path, title, user prompt"));
        assert!(outcome.prompt.ends_with(r"C:\\temp\\notes.pdf"));
        assert!(outcome.delegate_usage.is_empty());
    }

    #[tokio::test]
    async fn delegate_description_reports_usage_under_delegate_model() {
        let prompt = attachment_prompt(&[r"C:\\temp\\photo.png", r"C:\\temp\\notes.pdf"]);
        let files = vec![r"C:\\temp\\photo.png".to_owned(), r"C:\\temp\\notes.pdf".to_owned()];
        let describer = ScriptedDescriber {
            responses: Mutex::new(VecDeque::from([Ok((
                "A blue chart labelled Revenue".into(),
                TokenUsage {
                    input_tokens: 11,
                    output_tokens: 7,
                    ..Default::default()
                },
            ))])),
        };

        let outcome = rewrite_image_attachment_paths(prompt, &files, &vision_policy(), None, &describer).await;

        assert!(outcome.prompt.contains("<aionui-image-description>"));
        assert!(outcome.prompt.contains("A blue chart labelled Revenue"));
        assert!(outcome.prompt.ends_with(r"C:\\temp\\notes.pdf"));
        assert_eq!(
            outcome.delegate_usage,
            vec![DelegateUsageEventData {
                model: "vision-test".into(),
                input_tokens: 11,
                output_tokens: 7,
            }]
        );
    }

    #[tokio::test]
    async fn delegate_failure_is_honest_and_later_images_continue() {
        let prompt = attachment_prompt(&[r"C:\\temp\\first.png", r"C:\\temp\\second.png"]);
        let files = vec![r"C:\\temp\\first.png".to_owned(), r"C:\\temp\\second.png".to_owned()];
        let describer = ScriptedDescriber {
            responses: Mutex::new(VecDeque::from([
                Err("network timeout".into()),
                Ok(("Second image is readable".into(), TokenUsage::default())),
            ])),
        };

        let outcome = rewrite_image_attachment_paths(prompt, &files, &vision_policy(), None, &describer).await;

        assert!(outcome.prompt.contains("network timeout"));
        assert!(outcome.prompt.contains("Do NOT guess"));
        assert!(outcome.prompt.contains("Second image is readable"));
        assert_eq!(outcome.warnings.len(), 1);
        assert_eq!(outcome.delegate_usage.len(), 1);
    }

    #[tokio::test]
    async fn local_ocr_precedes_the_remote_vision_delegate() {
        let prompt = attachment_prompt(&[r"C:\\temp\\screenshot.png"]);
        let files = vec![r"C:\\temp\\screenshot.png".to_owned()];
        let describer = ScriptedDescriber {
            responses: Mutex::new(VecDeque::new()),
        };
        let local_ocr = LocalOcrSkill {
            name: "local-ocr-windows".into(),
            script_dir: r"C:\\One Work\\builtin-skills\\local-ocr-windows".into(),
            instructions: "Run the bundled local script.".into(),
        };

        let outcome =
            rewrite_image_attachment_paths(prompt, &files, &vision_policy(), Some(&local_ocr), &describer).await;

        assert!(outcome.prompt.contains("aionui-local-ocr-skill"));
        assert!(outcome.prompt.contains("ocr.ps1") || outcome.prompt.contains("local script"));
        assert!(
            outcome
                .prompt
                .contains(&format!("<image-path>{}</image-path>", files[0]))
        );
        assert!(outcome.delegate_usage.is_empty());
        assert!(outcome.warnings.is_empty());
    }
}
