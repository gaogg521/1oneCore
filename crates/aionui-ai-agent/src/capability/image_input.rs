use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use aion_types::message::ImageInputCapability;
use http::Uri;
use serde::Deserialize;
use tracing::error;

const IMAGE_INPUT_CATALOG_SCHEMA_VERSION: u32 = 1;
const IMAGE_INPUT_CATALOG_JSON: &str = include_str!("../../assets/model-capabilities/image_input_models.json");
const BEDROCK_INFERENCE_PROFILE_PREFIXES: [&str; 6] = ["us.", "eu.", "apac.", "au.", "jp.", "global."];

static IMAGE_INPUT_CATALOG: OnceLock<Option<ImageInputCatalog>> = OnceLock::new();

#[derive(Debug, Deserialize)]
struct ImageInputCatalog {
    schema_version: u32,
    providers: HashMap<String, ImageInputProvider>,
}

#[derive(Debug, Deserialize)]
struct ImageInputProvider {
    #[serde(default)]
    api: Option<String>,
    #[serde(default)]
    models: HashSet<String>,
}

pub(crate) fn resolve_image_input_capability(
    provider: &str,
    base_url: Option<&str>,
    model: &str,
) -> ImageInputCapability {
    embedded_catalog()
        .map(|catalog| resolve_from_catalog(catalog, provider, base_url, model))
        .unwrap_or(ImageInputCapability::Unknown)
}

fn embedded_catalog() -> Option<&'static ImageInputCatalog> {
    IMAGE_INPUT_CATALOG
        .get_or_init(|| match parse_catalog(IMAGE_INPUT_CATALOG_JSON) {
            Ok(catalog) => Some(catalog),
            Err(parse_error) => {
                error!(error = %parse_error, "Failed to parse embedded image input model catalog");
                None
            }
        })
        .as_ref()
}

fn parse_catalog(json: &str) -> Result<ImageInputCatalog, String> {
    let catalog = serde_json::from_str::<ImageInputCatalog>(json)
        .map_err(|parse_error| format!("invalid catalog JSON: {parse_error}"))?;
    if catalog.schema_version != IMAGE_INPUT_CATALOG_SCHEMA_VERSION {
        return Err(format!(
            "unsupported catalog schema version {}; expected {IMAGE_INPUT_CATALOG_SCHEMA_VERSION}",
            catalog.schema_version
        ));
    }
    if catalog.providers.is_empty() {
        return Err("catalog contains no providers".to_owned());
    }
    Ok(catalog)
}

enum ProviderLookup<'a> {
    /// Matched catalog / official providers by API root or builtin id.
    Providers(Vec<&'a str>),
    /// Valid http(s) base_url that matches no known provider (custom / private gateway).
    CustomGateway,
    /// Invalid URL, or no base_url with an unknown builtin provider.
    None,
}

fn resolve_from_catalog(
    catalog: &ImageInputCatalog,
    provider: &str,
    base_url: Option<&str>,
    model: &str,
) -> ImageInputCapability {
    match resolve_provider_lookup(catalog, provider, base_url) {
        ProviderLookup::Providers(provider_ids) => {
            for provider_id in provider_ids {
                let Some(candidate) = catalog.providers.get(provider_id) else {
                    continue;
                };
                if model_supports_image(candidate, model) {
                    return ImageInputCapability::Supported;
                }
            }
            ImageInputCapability::Unknown
        }
        // Custom LiteLLM / private OpenAI-compatible gateways reuse first-party model IDs
        // (often with hyphen/dot aliases). If the ID is already on the allowlist for any
        // verified provider, pass images through instead of stripping them.
        ProviderLookup::CustomGateway => {
            if catalog_lists_vision_model(catalog, model) {
                ImageInputCapability::Supported
            } else {
                ImageInputCapability::Unknown
            }
        }
        ProviderLookup::None => ImageInputCapability::Unknown,
    }
}

fn resolve_provider_lookup<'a>(
    catalog: &'a ImageInputCatalog,
    provider: &str,
    base_url: Option<&str>,
) -> ProviderLookup<'a> {
    if let Some(raw_base_url) = base_url {
        let Some(base_url) = normalize_api_root(raw_base_url) else {
            return ProviderLookup::None;
        };
        let matches = catalog
            .providers
            .iter()
            .filter_map(|(provider_id, candidate)| {
                let candidate_api = candidate.api.as_deref().and_then(normalize_api_root)?;
                api_roots_match(&base_url, &candidate_api).then_some(provider_id.as_str())
            })
            .collect::<Vec<_>>();
        if !matches.is_empty() {
            return ProviderLookup::Providers(matches);
        }

        if let Some(provider_id) = official_provider_id(provider, &base_url) {
            return ProviderLookup::Providers(vec![provider_id]);
        }
        return ProviderLookup::CustomGateway;
    }

    match builtin_provider_id(provider) {
        Some(provider_id) => ProviderLookup::Providers(vec![provider_id]),
        None => ProviderLookup::None,
    }
}

fn official_provider_id(provider: &str, api_root: &str) -> Option<&'static str> {
    let host = api_root.split('/').next()?;
    match (provider, host) {
        ("openai", "api.openai.com") => Some("openai"),
        ("openai", "generativelanguage.googleapis.com") => Some("google"),
        ("anthropic", "api.anthropic.com") => Some("anthropic"),
        ("vertex", _) => Some("google-vertex"),
        ("bedrock", _) => Some("amazon-bedrock"),
        _ => None,
    }
}

fn builtin_provider_id(provider: &str) -> Option<&'static str> {
    match provider {
        "openai" => Some("openai"),
        "anthropic" => Some("anthropic"),
        "vertex" => Some("google-vertex"),
        "bedrock" => Some("amazon-bedrock"),
        _ => None,
    }
}

fn catalog_lists_vision_model(catalog: &ImageInputCatalog, model: &str) -> bool {
    catalog
        .providers
        .values()
        .any(|provider| model_supports_image(provider, model))
}

fn model_supports_image(provider: &ImageInputProvider, model: &str) -> bool {
    let needle = model_match_keys(model);
    provider
        .models
        .iter()
        .any(|listed| !needle.is_disjoint(&model_match_keys(listed)))
}

fn normalize_model_id(model: &str) -> &str {
    let model = model.strip_prefix("models/").unwrap_or(model);
    BEDROCK_INFERENCE_PROFILE_PREFIXES
        .iter()
        .find_map(|prefix| {
            model
                .strip_prefix(prefix)
                .filter(|model| model.starts_with("anthropic."))
        })
        .unwrap_or(model)
}

/// Build loose match keys so common allowlisted multimodal ID spellings collide across vendors
/// (OpenAI / Anthropic / Moonshot / Qwen / GLM / DeepSeek-VL / MiniMax, …):
/// separators, case, `vendor/` prefix, `K2`↔`2` edition letter, trailing `YYYYMMDD` dates.
fn model_match_keys(model: &str) -> HashSet<String> {
    let model = normalize_model_id(model);
    let basename = model.rsplit('/').next().unwrap_or(model).trim();
    if basename.is_empty() {
        return HashSet::new();
    }

    let mut keys = HashSet::new();

    // Separator-preserving key: `.` / `_` / whitespace → `-`, collapse runs.
    let dashed = {
        let mut out = String::new();
        let mut prev_dash = false;
        for c in basename.chars() {
            let mapped = match c {
                '.' | '_' | ' ' | '\t' => '-',
                c => c.to_ascii_lowercase(),
            };
            if mapped == '-' {
                if !prev_dash && !out.is_empty() {
                    out.push('-');
                    prev_dash = true;
                }
            } else {
                out.push(mapped);
                prev_dash = false;
            }
        }
        while out.ends_with('-') {
            out.pop();
        }
        out
    };
    if !dashed.is_empty() {
        keys.insert(dashed.clone());
        if let Some(no_date) = strip_trailing_dashed_yyyymmdd(&dashed) {
            keys.insert(no_date);
        }
    }

    // Compact alphanumeric key (drops all separators / punctuation).
    let compact: String = basename
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    if compact.is_empty() {
        return keys;
    }
    insert_compact_variants(&mut keys, &compact);

    keys
}

fn insert_compact_variants(keys: &mut HashSet<String>, compact: &str) {
    keys.insert(compact.to_owned());
    if let Some(no_date) = strip_trailing_yyyymmdd(compact) {
        keys.insert(no_date.clone());
        if let Some(loose) = strip_edition_letter_before_version(&no_date) {
            keys.insert(loose);
        }
    }
    // Drop a single product letter between brand and version digits:
    // kimik26 → kimi26, minimaxm3 → minimax3 (brand must be long enough to avoid gpt4o → gp4o).
    if let Some(loose) = strip_edition_letter_before_version(compact) {
        keys.insert(loose.clone());
        if let Some(no_date) = strip_trailing_yyyymmdd(&loose) {
            keys.insert(no_date);
        }
    }
}

fn strip_edition_letter_before_version(compact: &str) -> Option<String> {
    let digit_pos = compact.find(|c: char| c.is_ascii_digit())?;
    if digit_pos < 5 {
        // Need brand (>=4 letters) + 1 edition letter before digits.
        return None;
    }
    let brand = &compact[..digit_pos - 1];
    let edition = compact.as_bytes()[digit_pos - 1];
    let version = &compact[digit_pos..];
    if brand.len() >= 4
        && brand.bytes().all(|b| b.is_ascii_lowercase())
        && edition.is_ascii_lowercase()
        && version.bytes().all(|b| b.is_ascii_digit() || b.is_ascii_lowercase())
    {
        return Some(format!("{brand}{version}"));
    }
    None
}

fn strip_trailing_yyyymmdd(compact: &str) -> Option<String> {
    if compact.len() <= 8 {
        return None;
    }
    let (head, tail) = compact.split_at(compact.len() - 8);
    if !tail.bytes().all(|b| b.is_ascii_digit()) || head.is_empty() {
        return None;
    }
    let year: u32 = tail.get(..4)?.parse().ok()?;
    if (2020..2040).contains(&year) {
        Some(head.to_owned())
    } else {
        None
    }
}

fn strip_trailing_dashed_yyyymmdd(dashed: &str) -> Option<String> {
    let (head, tail) = dashed.rsplit_once('-')?;
    if tail.len() == 8 && tail.bytes().all(|b| b.is_ascii_digit()) {
        let year: u32 = tail.get(..4)?.parse().ok()?;
        if (2020..2040).contains(&year) && !head.is_empty() {
            return Some(head.to_owned());
        }
    }
    None
}

fn normalize_api_root(raw_url: &str) -> Option<String> {
    let uri = raw_url.trim().parse::<Uri>().ok()?;
    match uri.scheme_str()? {
        "http" | "https" => {}
        _ => return None,
    }
    let authority = uri.authority()?.as_str().to_ascii_lowercase();
    let mut path = uri.path().trim_end_matches('/').to_owned();
    for suffix in ["/chat/completions", "/responses", "/messages"] {
        if let Some(prefix) = path.strip_suffix(suffix) {
            path = prefix.trim_end_matches('/').to_owned();
            break;
        }
    }
    Some(format!("{authority}{path}"))
}

fn api_roots_match(left: &str, right: &str) -> bool {
    left == right || strip_version_suffix(left) == Some(right) || strip_version_suffix(right) == Some(left)
}

fn strip_version_suffix(api_root: &str) -> Option<&str> {
    api_root.strip_suffix("/v1")
}

#[cfg(test)]
#[path = "image_input_test.rs"]
mod image_input_test;
