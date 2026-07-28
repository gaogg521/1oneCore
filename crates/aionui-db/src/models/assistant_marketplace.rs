//! Row model for the `assistant_marketplace_personas` table.

use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

/// A catalog entry in the expert marketplace — a browsable persona template
/// that is NOT a real assistant until a user explicitly installs it. See
/// `IAssistantMarketplaceRepository`.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct MarketplacePersonaRow {
    pub id: String,
    pub source: String,
    pub name: String,
    pub description: Option<String>,
    pub rule_content: String,
    pub display_name: Option<String>,
    pub role_name: Option<String>,
    pub category: Option<String>,
    pub has_avatar: bool,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

/// Insert-or-update parameters for `assistant_marketplace_personas`.
#[derive(Debug, Clone)]
pub struct UpsertMarketplacePersonaParams<'a> {
    pub id: &'a str,
    pub source: &'a str,
    pub name: &'a str,
    pub description: Option<&'a str>,
    pub rule_content: &'a str,
    pub display_name: Option<&'a str>,
    pub role_name: Option<&'a str>,
    pub category: Option<&'a str>,
    pub has_avatar: bool,
}
