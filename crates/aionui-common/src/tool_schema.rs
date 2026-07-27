//! Compatibility checks for MCP tool definitions before they reach a model API.
//!
//! # Why this exists
//!
//! MCP servers are free-form: anyone can publish one, and nothing in the MCP
//! protocol constrains what a tool's `inputSchema` may look like. Model APIs
//! are stricter. Anthropic's Messages API rejects the *entire request* when
//! any tool carries a property key outside `^[a-zA-Z0-9_.-]{1,64}$` — a
//! single non-conforming tool from one server takes down every conversation,
//! with an error that only identifies the offender by its index in the
//! flattened tool array (`tools.214.custom.input_schema.properties`).
//!
//! A real-world example that motivated this module: a financial-data MCP
//! server declared a parameterless tool as
//! `properties: { "（无业务参数）": { "type": "string" } }` — using a
//! human-readable placeholder where an empty object belongs. The MCP
//! handshake succeeded, `tools/list` succeeded, the server reported
//! "Connected", and every subsequent message failed with HTTP 400.
//!
//! Validating here lets the app (a) tell the user exactly which tool and
//! which key is at fault, and (b) drop just that tool instead of losing the
//! server — or the conversation.

use serde_json::Value;

/// Maximum length of a tool parameter (property) name.
const MAX_PROPERTY_KEY_LEN: usize = 64;

/// Maximum length of a tool name.
const MAX_TOOL_NAME_LEN: usize = 128;

/// Where an incompatibility was found and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaIncompatibility {
    /// JSON path within the tool definition, e.g.
    /// `inputSchema.properties.（无业务参数）`.
    pub path: String,
    /// The offending key (property name, or the tool name itself).
    pub key: String,
    /// Why it was rejected, in user-facing terms.
    pub reason: IncompatibilityReason,
}

/// The specific rule a key violated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IncompatibilityReason {
    /// Contains characters outside the allowed set.
    IllegalCharacters,
    /// Exceeds the maximum allowed length.
    TooLong,
    /// Empty string.
    Empty,
}

impl IncompatibilityReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::IllegalCharacters => "illegal_characters",
            Self::TooLong => "too_long",
            Self::Empty => "empty",
        }
    }
}

/// Check a tool *parameter* name against the model API's property-key rule:
/// `^[a-zA-Z0-9_.-]{1,64}$`.
fn check_property_key(key: &str) -> Option<IncompatibilityReason> {
    if key.is_empty() {
        return Some(IncompatibilityReason::Empty);
    }
    // Length is counted in characters, and any non-ASCII character is already
    // an illegal-character violation, so `chars().count()` and byte length
    // agree for every input that could otherwise pass.
    if !key.chars().all(is_allowed_property_char) {
        return Some(IncompatibilityReason::IllegalCharacters);
    }
    if key.len() > MAX_PROPERTY_KEY_LEN {
        return Some(IncompatibilityReason::TooLong);
    }
    None
}

/// Check a *tool* name. Tool names allow the same characters as property
/// keys minus `.`, and permit a longer maximum.
fn check_tool_name(name: &str) -> Option<IncompatibilityReason> {
    if name.is_empty() {
        return Some(IncompatibilityReason::Empty);
    }
    if !name.chars().all(is_allowed_tool_name_char) {
        return Some(IncompatibilityReason::IllegalCharacters);
    }
    if name.len() > MAX_TOOL_NAME_LEN {
        return Some(IncompatibilityReason::TooLong);
    }
    None
}

fn is_allowed_property_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')
}

fn is_allowed_tool_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-')
}

/// Validate one tool definition.
///
/// `name` is the tool's registered name; `input_schema` is its JSON Schema
/// (`None` is trivially valid — a tool with no declared parameters).
///
/// Returns every problem found rather than stopping at the first, so the UI
/// can show a complete picture instead of making the user fix issues one
/// round-trip at a time.
pub fn validate_tool(name: &str, input_schema: Option<&Value>) -> Vec<SchemaIncompatibility> {
    let mut found = Vec::new();

    if let Some(reason) = check_tool_name(name) {
        found.push(SchemaIncompatibility {
            path: "name".to_owned(),
            key: name.to_owned(),
            reason,
        });
    }

    if let Some(schema) = input_schema {
        walk_schema(schema, "inputSchema", &mut found);
    }

    found
}

/// Convenience predicate: is this tool safe to forward to the model API?
pub fn is_tool_compatible(name: &str, input_schema: Option<&Value>) -> bool {
    validate_tool(name, input_schema).is_empty()
}

/// Recursively collect illegal property keys from a JSON Schema node.
///
/// Walks the composition keywords a real MCP server is likely to emit
/// (`properties`, `items`, `anyOf`/`oneOf`/`allOf`, `$defs`/`definitions`)
/// because the API validates the fully-resolved schema, not just its top
/// level — a bad key nested inside an array item is just as fatal.
fn walk_schema(schema: &Value, path: &str, found: &mut Vec<SchemaIncompatibility>) {
    let Some(obj) = schema.as_object() else {
        return;
    };

    if let Some(props) = obj.get("properties").and_then(Value::as_object) {
        for (key, child) in props {
            if let Some(reason) = check_property_key(key) {
                found.push(SchemaIncompatibility {
                    path: format!("{path}.properties"),
                    key: key.clone(),
                    reason,
                });
            }
            walk_schema(child, &format!("{path}.properties.{key}"), found);
        }
    }

    if let Some(items) = obj.get("items") {
        walk_schema(items, &format!("{path}.items"), found);
    }

    for keyword in ["anyOf", "oneOf", "allOf"] {
        if let Some(branches) = obj.get(keyword).and_then(Value::as_array) {
            for (index, branch) in branches.iter().enumerate() {
                walk_schema(branch, &format!("{path}.{keyword}[{index}]"), found);
            }
        }
    }

    // `$defs`/`definitions` keys are reference targets, not parameter names,
    // so only their contents are checked — not the keys themselves.
    for keyword in ["$defs", "definitions"] {
        if let Some(defs) = obj.get(keyword).and_then(Value::as_object) {
            for (name, def) in defs {
                walk_schema(def, &format!("{path}.{keyword}.{name}"), found);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -- property key rules ---------------------------------------------------

    #[test]
    fn plain_ascii_key_is_allowed() {
        assert_eq!(check_property_key("codes"), None);
        assert_eq!(check_property_key("start_date"), None);
        assert_eq!(check_property_key("v1.2-beta"), None);
        assert_eq!(check_property_key("A0_.-"), None);
    }

    #[test]
    fn chinese_key_is_illegal() {
        assert_eq!(
            check_property_key("股票代码"),
            Some(IncompatibilityReason::IllegalCharacters)
        );
    }

    #[test]
    fn fullwidth_parenthesis_key_is_illegal() {
        // The exact shape that took down a real conversation.
        assert_eq!(
            check_property_key("（无业务参数）"),
            Some(IncompatibilityReason::IllegalCharacters)
        );
    }

    #[test]
    fn space_and_punctuation_keys_are_illegal() {
        assert_eq!(
            check_property_key("start date"),
            Some(IncompatibilityReason::IllegalCharacters)
        );
        assert_eq!(
            check_property_key("code(required)"),
            Some(IncompatibilityReason::IllegalCharacters)
        );
        assert_eq!(
            check_property_key("a/b"),
            Some(IncompatibilityReason::IllegalCharacters)
        );
    }

    #[test]
    fn empty_key_is_rejected() {
        assert_eq!(check_property_key(""), Some(IncompatibilityReason::Empty));
    }

    #[test]
    fn key_at_length_limit_is_allowed_and_over_is_rejected() {
        let at_limit = "a".repeat(MAX_PROPERTY_KEY_LEN);
        assert_eq!(check_property_key(&at_limit), None);

        let over_limit = "a".repeat(MAX_PROPERTY_KEY_LEN + 1);
        assert_eq!(check_property_key(&over_limit), Some(IncompatibilityReason::TooLong));
    }

    // -- tool name rules ------------------------------------------------------

    #[test]
    fn tool_name_allows_underscore_and_dash_but_not_dot() {
        assert_eq!(check_tool_name("get_a_share_quotes"), None);
        assert_eq!(check_tool_name("stock-sdk"), None);
        assert_eq!(
            check_tool_name("stock.sdk"),
            Some(IncompatibilityReason::IllegalCharacters)
        );
    }

    #[test]
    fn tool_name_rejects_non_ascii() {
        assert_eq!(
            check_tool_name("获取行情"),
            Some(IncompatibilityReason::IllegalCharacters)
        );
    }

    #[test]
    fn tool_name_over_limit_is_rejected() {
        let over = "a".repeat(MAX_TOOL_NAME_LEN + 1);
        assert_eq!(check_tool_name(&over), Some(IncompatibilityReason::TooLong));
    }

    // -- validate_tool --------------------------------------------------------

    #[test]
    fn valid_tool_reports_nothing() {
        let schema = json!({
            "type": "object",
            "properties": {
                "codes": { "type": "array", "items": { "type": "string" } }
            },
            "required": ["codes"]
        });
        assert!(validate_tool("get_a_share_quotes", Some(&schema)).is_empty());
        assert!(is_tool_compatible("get_a_share_quotes", Some(&schema)));
    }

    #[test]
    fn tool_without_schema_is_valid() {
        assert!(validate_tool("noop", None).is_empty());
    }

    #[test]
    fn empty_properties_object_is_valid() {
        // The correct way to express "this tool takes no parameters".
        let schema = json!({ "type": "object", "properties": {}, "required": [] });
        assert!(validate_tool("ft_goodwill_market_overview", Some(&schema)).is_empty());
    }

    #[test]
    fn detects_the_real_world_placeholder_property() {
        let schema = json!({
            "type": "object",
            "additionalProperties": false,
            "required": [],
            "properties": {
                "（无业务参数）": { "type": "string", "maxLength": 4096 }
            }
        });
        let found = validate_tool("ft_goodwill_market_overview", Some(&schema));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].key, "（无业务参数）");
        assert_eq!(found[0].path, "inputSchema.properties");
        assert_eq!(found[0].reason, IncompatibilityReason::IllegalCharacters);
        assert!(!is_tool_compatible("ft_goodwill_market_overview", Some(&schema)));
    }

    #[test]
    fn detects_nested_property_inside_object() {
        let schema = json!({
            "type": "object",
            "properties": {
                "filter": {
                    "type": "object",
                    "properties": { "开始日期": { "type": "string" } }
                }
            }
        });
        let found = validate_tool("query", Some(&schema));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].key, "开始日期");
        assert_eq!(found[0].path, "inputSchema.properties.filter.properties");
    }

    #[test]
    fn detects_property_inside_array_items() {
        let schema = json!({
            "type": "object",
            "properties": {
                "rows": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": { "字段": { "type": "string" } }
                    }
                }
            }
        });
        let found = validate_tool("bulk", Some(&schema));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, "inputSchema.properties.rows.items.properties");
    }

    #[test]
    fn detects_property_inside_any_of_branch() {
        let schema = json!({
            "type": "object",
            "properties": {
                "target": {
                    "anyOf": [
                        { "type": "string" },
                        { "type": "object", "properties": { "代码": { "type": "string" } } }
                    ]
                }
            }
        });
        let found = validate_tool("lookup", Some(&schema));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, "inputSchema.properties.target.anyOf[1].properties");
    }

    #[test]
    fn detects_property_inside_defs() {
        let schema = json!({
            "type": "object",
            "properties": { "ref": { "$ref": "#/$defs/Filter" } },
            "$defs": {
                "Filter": { "type": "object", "properties": { "结束日期": { "type": "string" } } }
            }
        });
        let found = validate_tool("with_defs", Some(&schema));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, "inputSchema.$defs.Filter.properties");
    }

    #[test]
    fn def_names_themselves_are_not_validated() {
        // `$defs` keys are reference targets, never sent as parameter names.
        let schema = json!({
            "type": "object",
            "properties": {},
            "$defs": { "过滤器": { "type": "object", "properties": {} } }
        });
        assert!(validate_tool("ok", Some(&schema)).is_empty());
    }

    #[test]
    fn reports_every_problem_not_just_the_first() {
        let schema = json!({
            "type": "object",
            "properties": {
                "股票代码": { "type": "string" },
                "开始日期": { "type": "string" },
                "end_date": { "type": "string" }
            }
        });
        let found = validate_tool("查询", Some(&schema));
        // 1 bad tool name + 2 bad property keys; `end_date` is fine.
        assert_eq!(found.len(), 3);
        assert!(found.iter().any(|f| f.path == "name" && f.key == "查询"));
        assert!(found.iter().any(|f| f.key == "股票代码"));
        assert!(found.iter().any(|f| f.key == "开始日期"));
        assert!(!found.iter().any(|f| f.key == "end_date"));
    }

    #[test]
    fn non_object_schema_is_ignored_rather_than_panicking() {
        // Defensive: a malformed server could send anything here.
        assert!(validate_tool("weird", Some(&json!("not an object"))).is_empty());
        assert!(validate_tool("weird", Some(&json!(42))).is_empty());
        assert!(validate_tool("weird", Some(&json!(null))).is_empty());
    }

    #[test]
    fn reason_strings_are_stable() {
        assert_eq!(IncompatibilityReason::IllegalCharacters.as_str(), "illegal_characters");
        assert_eq!(IncompatibilityReason::TooLong.as_str(), "too_long");
        assert_eq!(IncompatibilityReason::Empty.as_str(), "empty");
    }
}
