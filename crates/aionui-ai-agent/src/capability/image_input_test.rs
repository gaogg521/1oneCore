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
    assert_eq!(
        resolve_from_catalog(
            &catalog,
            "openai",
            Some("https://litellm-internal.example.com/v1"),
            "deepseek-v4-flash",
        ),
        ImageInputCapability::Unknown
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

    // 纯文本 / 未入白名单：即使同厂系列也不放行
    for rejected in ["deepseek-v4-flash", "minimax-2-7", "gpt-3.5-turbo"] {
        assert_eq!(
            resolve_from_catalog(&catalog, "openai", Some(gateway), rejected),
            ImageInputCapability::Unknown,
            "{rejected} must stay Unknown"
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

#[test]
fn unknown_provider_or_model_fails_closed_as_unknown() {
    let catalog = catalog();

    // Custom gateway + allowlisted vision model ID → Supported (pass-through).
    assert_eq!(
        resolve_from_catalog(&catalog, "openai", Some("https://private.example/v1"), "gpt-4o",),
        ImageInputCapability::Supported
    );
    // Custom gateway + unknown model ID still fails closed.
    assert_eq!(
        resolve_from_catalog(
            &catalog,
            "openai",
            Some("https://private.example/v1"),
            "totally-unknown-model",
        ),
        ImageInputCapability::Unknown
    );
    assert_eq!(
        resolve_from_catalog(&catalog, "openai", Some("https://api.openai.com/v1"), "missing-model"),
        ImageInputCapability::Unknown
    );
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
        resolve_image_input_capability(
            "openai",
            Some("https://litellm-internal.123u.com/v1"),
            "kimi-k2-6",
        ),
        ImageInputCapability::Supported
    );
}
