use aion_types::message::ImageInputCapability;
use serde_json::json;

use super::{
    IMAGE_INPUT_CATALOG_JSON, ImageInputCatalog, parse_catalog, resolve_from_catalog, resolve_image_input_capability,
};

fn catalog() -> ImageInputCatalog {
    serde_json::from_value(json!({
        "schema_version": 1,
        "providers": {
            "openai": {
                "models": ["gpt-4o", "gpt-4o-2024-11-20"]
            },
            "google": {
                "models": ["gemini-2.5-flash"]
            },
            "anthropic": {
                "api": "https://api.anthropic.com",
                "models": ["claude-sonnet-4-5-20250929"]
            },
            "dashscope": {
                "api": "https://dashscope.aliyuncs.com/compatible-mode/v1",
                "models": ["qwen3.7-plus", "qwen3-vl-plus"]
            },
            "moonshot-global": {
                "api": "https://api.moonshot.ai/v1",
                "models": ["kimi-k2.6"]
            },
            "zhipu": {
                "api": "https://open.bigmodel.cn/api/paas/v4",
                "models": ["glm-4.6v"]
            },
            "qianfan": {
                "api": "https://qianfan.baidubce.com/v2",
                "models": ["deepseek-vl2"]
            },
            "openrouter": {
                "api": "https://openrouter.ai/api/v1",
                "models": []
            },
            "amazon-bedrock": {
                "models": ["anthropic.claude-sonnet-4-20250514-v1:0"]
            },
            "deepseek": {
                "api": "https://api.deepseek.com",
                "models": ["deepseek-vl"]
            }
        }
    }))
    .expect("valid catalog fixture")
}

#[test]
fn embedded_allowlist_is_valid_and_contains_regression_provider() {
    let catalog = parse_catalog(IMAGE_INPUT_CATALOG_JSON).expect("valid embedded catalog");

    assert!(catalog.providers.contains_key("dashscope"));
    assert!(catalog.providers.contains_key("moonshot-global"));
}

#[test]
fn rejects_unknown_catalog_schema_version() {
    let error = parse_catalog(r#"{"schema_version":2,"providers":{"openai":{"models":["gpt-4o"]}}}"#)
        .expect_err("unknown schemas must fail closed");

    assert!(error.contains("unsupported catalog schema version 2"));
}

#[test]
fn embedded_allowlist_resolves_regression_models_without_network() {
    assert_eq!(
        resolve_image_input_capability(
            "openai",
            Some("https://dashscope.aliyuncs.com/compatible-mode/v1"),
            "qwen3.7-plus",
        ),
        ImageInputCapability::Supported
    );
    assert_eq!(
        resolve_image_input_capability(
            "openai",
            Some("https://dashscope.aliyuncs.com/compatible-mode/v1"),
            "kimi-k2.6",
        ),
        ImageInputCapability::Unknown
    );
    assert_eq!(
        resolve_image_input_capability("openai", Some("https://api.moonshot.ai/v1"), "kimi-k2.6"),
        ImageInputCapability::Supported
    );
}

#[test]
fn embedded_allowlist_resolves_official_kimi_k2_7_code() {
    for base_url in ["https://api.moonshot.cn/v1", "https://api.moonshot.ai/v1"] {
        assert_eq!(
            resolve_image_input_capability("openai", Some(base_url), "kimi-k2.7-code"),
            ImageInputCapability::Supported
        );
    }
}

#[test]
fn resolves_supported_and_unlisted_models_on_the_same_provider() {
    let catalog = catalog();

    assert_eq!(
        resolve_from_catalog(
            &catalog,
            "openai",
            Some("https://dashscope.aliyuncs.com/compatible-mode/v1"),
            "qwen3.7-plus",
        ),
        ImageInputCapability::Supported
    );
    assert_eq!(
        resolve_from_catalog(
            &catalog,
            "openai",
            Some("https://dashscope.aliyuncs.com/compatible-mode/v1"),
            "kimi-k2.6",
        ),
        ImageInputCapability::Unknown
    );
}

#[test]
fn resolves_same_model_id_by_provider_api_not_model_name_alone() {
    let catalog = catalog();

    assert_eq!(
        resolve_from_catalog(&catalog, "openai", Some("https://api.moonshot.ai/v1"), "kimi-k2.6",),
        ImageInputCapability::Supported
    );
    // Known aggregator with empty vision list: do not invent support from the model name alone.
    assert_eq!(
        resolve_from_catalog(&catalog, "openai", Some("https://openrouter.ai/api/v1"), "kimi-k2.6",),
        ImageInputCapability::Unknown
    );
}

#[test]
fn custom_gateway_matches_allowlisted_model_aliases() {
    let catalog = catalog();

    for alias in [
        "kimi-k2-6",
        "kimi-k2.6",
        "kimi2-6",
        "kimi-2-6",
        "kimi_k2.6",
        "Kimi K2.6",
        "moonshotai/kimi-k2.6",
    ] {
        assert_eq!(
            resolve_from_catalog(
                &catalog,
                "openai",
                Some("https://litellm-internal.example.com/v1"),
                alias,
            ),
            ImageInputCapability::Supported,
            "alias {alias} should match catalog kimi-k2.6"
        );
    }
    // A custom gateway can't be identified by name, so an unmatched model
    // defaults to Supported rather than silently stripping every image sent
    // to it — see the `ProviderLookup::CustomGateway` arm in `resolve_from_catalog`.
    assert_eq!(
        resolve_from_catalog(
            &catalog,
            "openai",
            Some("https://litellm-internal.example.com/v1"),
            "deepseek-v4-flash",
        ),
        ImageInputCapability::Supported
    );
}

#[test]
fn cross_vendor_common_spellings_match_allowlist_on_custom_gateway() {
    let catalog = catalog();
    let gateway = "https://litellm-internal.example.com/v1";

    let cases = [
        // OpenAI / GPT
        ("gpt-4o", "GPT4o"),
        ("gpt-4o", "gpt4o"),
        ("gpt-4o", "gpt-4o-2024-11-20"),
        // Anthropic / Claude (dated catalog id ↔ undated / dotted)
        ("claude-sonnet-4-5-20250929", "claude-sonnet-4-5"),
        ("claude-sonnet-4-5-20250929", "claude-sonnet-4.5"),
        ("claude-sonnet-4-5-20250929", "Claude Sonnet 4.5"),
        // Moonshot / Kimi
        ("kimi-k2.6", "kimi2-6"),
        ("kimi-k2.6", "kimi-2-6"),
        // Qwen
        ("qwen3-vl-plus", "qwen3.vl.plus"),
        ("qwen3-vl-plus", "Qwen3-VL-Plus"),
        ("qwen3-vl-plus", "qwen/qwen3-vl-plus"),
        // GLM
        ("glm-4.6v", "glm-4-6v"),
        ("glm-4.6v", "GLM4.6V"),
        ("glm-4.6v", "glm46v"),
        // DeepSeek-VL（白名单内的视觉款，不是 v4-flash 纯文本）
        ("deepseek-vl2", "deepseek-vl-2"),
        ("deepseek-vl2", "DeepSeek-VL2"),
        ("deepseek-vl", "deepseek_vl"),
    ];

    for (canonical, alias) in cases {
        assert_eq!(
            resolve_from_catalog(&catalog, "openai", Some(gateway), alias),
            ImageInputCapability::Supported,
            "{alias} should match allowlisted {canonical}"
        );
    }

    // Same-vendor lookalikes that aren't in the vision allowlist (e.g. a text-only
    // sibling model) still resolve to Supported here — not because the catalog
    // identified them as vision-capable, but because they're on a custom gateway,
    // which defaults to Supported for anything it can't name-match either way. See
    // `custom_gateway_unmatched_models_default_to_supported` for that invariant.
    for unmatched in ["deepseek-v4-flash", "minimax-2-7", "gpt-3.5-turbo"] {
        assert_eq!(
            resolve_from_catalog(&catalog, "openai", Some(gateway), unmatched),
            ImageInputCapability::Supported,
            "{unmatched} is unmatched but still defaults to Supported on a custom gateway"
        );
    }
}

#[test]
fn normalizes_bedrock_inference_profile_prefixes() {
    let catalog = catalog();

    for model in [
        "anthropic.claude-sonnet-4-20250514-v1:0",
        "us.anthropic.claude-sonnet-4-20250514-v1:0",
        "global.anthropic.claude-sonnet-4-20250514-v1:0",
    ] {
        assert_eq!(
            resolve_from_catalog(&catalog, "bedrock", None, model),
            ImageInputCapability::Supported
        );
    }
}

#[test]
fn normalizes_full_endpoint_and_optional_v1_suffix() {
    let catalog = catalog();

    assert_eq!(
        resolve_from_catalog(
            &catalog,
            "openai",
            Some("https://api.deepseek.com/v1/chat/completions?trace=1"),
            "deepseek-vl",
        ),
        ImageInputCapability::Supported
    );
}

#[test]
fn maps_official_provider_hosts_without_catalog_api_urls() {
    let catalog = catalog();

    assert_eq!(
        resolve_from_catalog(&catalog, "openai", Some("https://api.openai.com/v1"), "gpt-4o",),
        ImageInputCapability::Supported
    );
    assert_eq!(
        resolve_from_catalog(
            &catalog,
            "openai",
            Some("https://generativelanguage.googleapis.com/v1beta/openai"),
            "models/gemini-2.5-flash",
        ),
        ImageInputCapability::Supported
    );
}

/// Whether an unmatched model fails closed (Unknown) or defaults open
/// (Supported) depends on *why* it went unmatched:
/// - A recognized official provider (its API root matched a catalog entry) has
///   a real, enumerable model list — a miss there means the model genuinely
///   isn't on it, so it fails closed.
/// - A custom/private gateway can't be identified by name at all, so a miss
///   says nothing about the real backend model — it defaults open instead (see
///   the `ProviderLookup::CustomGateway` arm in `resolve_from_catalog`).
/// - An unparseable base_url can't be resolved either way and fails closed.
#[test]
fn unmatched_model_resolution_depends_on_whether_the_provider_is_identifiable() {
    let catalog = catalog();

    // Custom gateway + allowlisted vision model ID → Supported (pass-through).
    assert_eq!(
        resolve_from_catalog(&catalog, "openai", Some("https://private.example/v1"), "gpt-4o",),
        ImageInputCapability::Supported
    );
    // Custom gateway + unrecognized model ID → defaults open (see module docs above).
    assert_eq!(
        resolve_from_catalog(
            &catalog,
            "openai",
            Some("https://private.example/v1"),
            "totally-unknown-model",
        ),
        ImageInputCapability::Supported
    );
    // Recognized official provider (openai.com) + a model not on its list → fails closed.
    assert_eq!(
        resolve_from_catalog(&catalog, "openai", Some("https://api.openai.com/v1"), "missing-model"),
        ImageInputCapability::Unknown
    );
    // Unparseable base_url → fails closed.
    assert_eq!(
        resolve_from_catalog(&catalog, "openai", Some("not-a-url"), "gpt-4o"),
        ImageInputCapability::Unknown
    );
}

#[test]
fn embedded_allowlist_resolves_kimi_hyphen_alias_on_moonshot() {
    assert_eq!(
        resolve_image_input_capability("openai", Some("https://api.moonshot.ai/v1"), "kimi-k2-6"),
        ImageInputCapability::Supported
    );
    assert_eq!(
        resolve_image_input_capability("openai", Some("https://litellm-internal.123u.com/v1"), "kimi-k2-6",),
        ImageInputCapability::Supported
    );
}

/// A private/custom gateway (e.g. an operator-run LiteLLM instance fronting
/// arbitrary provider aliases such as `glm-latest`, `deepseek-pro-latest`)
/// can't be identified by name, so the catalog has no way to confirm *or*
/// rule out vision support for a model it doesn't recognize on one. Prior to
/// this test's introduction, an unmatched model on such a gateway defaulted
/// to `Unknown`, which the engine treats as "strip every attached image" —
/// in practice this permanently blacked out vision for any operator-aliased
/// model, including genuinely vision-capable ones, with no error surfaced to
/// the user. The default was flipped to `Supported`: let the provider itself
/// reject an image it truly can't handle, rather than the client silently
/// discarding it. These specific models (real text-only siblings of vision
/// models — MiniMax vision is the M3 line not M2.7, DeepSeek vision is the
/// VL/OCR line not v4-flash) are the ones the old fail-closed test guarded;
/// this asserts the new default explicitly so a future revert is deliberate,
/// not accidental.
#[test]
fn custom_gateway_unmatched_models_default_to_supported() {
    let gateway = Some("https://litellm-internal.123u.com/v1");
    for model in [
        "minimax-2-7",
        "MiniMax-M2.7",
        "minimax2-7",
        "deepseek-v4-flash",
        "deepseek-v4",
        "deepseek-v4-flash-2024-11-20",
        "glm-latest",
        "deepseek-pro-latest",
    ] {
        assert_eq!(
            resolve_image_input_capability("openai", gateway, model),
            ImageInputCapability::Supported,
            "{model} is unmatched on a custom gateway and must default open, not silently strip images"
        );
    }
}
