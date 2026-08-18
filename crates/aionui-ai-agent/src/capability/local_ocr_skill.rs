//! Host-platform local OCR skill selection for bridged text-only sessions.
//!
//! These names are bundled with the application under `builtin-skills/`.
//! The host skill is added to the normal skill snapshot instead of placing
//! all platform variants in `auto-inject/`: a Windows session must never be
//! instructed to run a macOS or Linux command.

/// Return the bundled local-OCR skill appropriate for the host platform.
///
/// `None` deliberately means an unsupported platform. Callers must retain
/// their safe image-unavailable path rather than guessing at a shell command.
pub const fn host_local_ocr_skill_name() -> Option<&'static str> {
    if cfg!(target_os = "windows") {
        Some("local-ocr-windows")
    } else if cfg!(target_os = "macos") {
        Some("local-ocr-macos")
    } else if cfg!(target_os = "linux") {
        Some("local-ocr-linux")
    } else {
        None
    }
}

/// Add the host OCR skill to a conversation's selected skills exactly once.
///
/// The user-visible snapshot remains intact; this is the platform default
/// that makes local OCR available without an assistant-by-assistant opt-in.
pub fn with_host_local_ocr_skill(configured_skills: &[String]) -> Vec<String> {
    let mut skills = configured_skills.to_vec();
    if let Some(name) = host_local_ocr_skill_name()
        && !skills.iter().any(|skill| skill == name)
    {
        skills.push(name.to_owned());
    }
    skills
}

/// Recognize the bundled platform skill names. Keeping this exact avoids
/// treating an unrelated skill that merely mentions an image as an OCR tool.
pub fn is_bundled_local_ocr_skill(name: &str) -> bool {
    matches!(name, "local-ocr-windows" | "local-ocr-macos" | "local-ocr-linux")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_skill_is_added_once() {
        let input = vec!["pdf".to_owned()];
        let output = with_host_local_ocr_skill(&input);
        assert!(output.starts_with(&input));
        if let Some(name) = host_local_ocr_skill_name() {
            assert_eq!(output.iter().filter(|item| item.as_str() == name).count(), 1);
            assert_eq!(with_host_local_ocr_skill(&output), output);
        }
    }

    #[test]
    fn only_known_platform_names_are_classified_as_bundled_ocr() {
        assert!(is_bundled_local_ocr_skill("local-ocr-windows"));
        assert!(is_bundled_local_ocr_skill("local-ocr-macos"));
        assert!(is_bundled_local_ocr_skill("local-ocr-linux"));
        assert!(!is_bundled_local_ocr_skill("image-helper"));
    }
}
