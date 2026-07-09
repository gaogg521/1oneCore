//! Requirement breakdown (A1 L2): build the prompt that asks a digital
//! employee to split an epic/feature into child requirements, and parse the
//! agent's reply back into structured child items.
//!
//! Parsing is deliberately lenient: LLMs wrap JSON in prose or ```json fences,
//! so we extract the outermost array and clamp each field to the allowed
//! enum sets rather than rejecting the whole batch on one bad value.

use crate::models::{REQUIREMENT_PRIORITIES, REQUIREMENT_TYPES, RequirementRow};

/// Upper bound on children created from one breakdown, guarding against a
/// runaway reply.
const MAX_CHILDREN: usize = 20;

/// A parsed child requirement, fields already clamped to valid enum values.
#[derive(Debug, Clone, PartialEq)]
pub struct BreakdownItem {
    pub subject: String,
    pub description: Option<String>,
    pub kind: String,
    pub priority: String,
}

/// Compose the breakdown instruction from the parent requirement. Plain text
/// so any agent backend can consume it; the JSON-only contract is spelled out
/// explicitly because parsing depends on it.
pub fn build_breakdown_prompt(req: &RequirementRow) -> String {
    let mut out = String::new();
    out.push_str("你是一名资深研发规划助手。请把下面这条协作看板需求拆解为若干条更小、可独立开发的子需求。\n\n");
    out.push_str(&format!("标题：{}\n", req.subject));
    out.push_str(&format!("类型：{} · 优先级：{}\n", req.r#type, req.priority));
    if let Some(desc) = req.description.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        out.push_str(&format!("\n描述：\n{desc}\n"));
    }
    out.push_str(
        "\n要求：\n\
         - 每条子需求聚焦一个明确、可交付的工作单元\n\
         - 数量控制在 2-8 条，不要过度拆分\n\
         - type 取值只能是 story 或 task；priority 取值只能是 low/medium/high/urgent\n\
         - 严格只输出一个 JSON 数组，不要任何解释文字，也不要 Markdown 代码块围栏\n\
         - 数组每个元素形如：\
         {\"subject\":\"子需求标题\",\"description\":\"简要说明\",\"type\":\"story\",\"priority\":\"medium\"}\n",
    );
    out
}

/// Extract child items from an agent reply. Returns an empty vec when nothing
/// parseable is found (caller treats that as a breakdown failure).
pub fn parse_breakdown_items(reply: &str) -> Vec<BreakdownItem> {
    let Some(array_slice) = extract_json_array(reply) else {
        return Vec::new();
    };
    let Ok(serde_json::Value::Array(elems)) = serde_json::from_str::<serde_json::Value>(array_slice) else {
        return Vec::new();
    };

    let mut items = Vec::new();
    for elem in elems {
        let Some(obj) = elem.as_object() else { continue };
        let subject = obj
            .get("subject")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let Some(subject) = subject else { continue };

        let description = obj
            .get("description")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let kind = clamp(obj.get("type").and_then(|v| v.as_str()), REQUIREMENT_TYPES, "story");
        let priority = clamp(
            obj.get("priority").and_then(|v| v.as_str()),
            REQUIREMENT_PRIORITIES,
            "medium",
        );

        items.push(BreakdownItem {
            subject: subject.to_owned(),
            description,
            kind,
            priority,
        });
        if items.len() >= MAX_CHILDREN {
            break;
        }
    }
    items
}

/// Return the value if it is in `allowed`, else `default`.
fn clamp(value: Option<&str>, allowed: &[&str], default: &str) -> String {
    match value.map(str::trim) {
        Some(v) if allowed.contains(&v) => v.to_owned(),
        _ => default.to_owned(),
    }
}

/// Slice out the outermost `[ ... ]` array from a reply that may contain prose
/// or code fences around it.
fn extract_json_array(reply: &str) -> Option<&str> {
    let start = reply.find('[')?;
    let end = reply.rfind(']')?;
    if end > start { Some(&reply[start..=end]) } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_array() {
        let reply = r#"[
            {"subject":"设计接口","description":"定义 REST","type":"story","priority":"high"},
            {"subject":"实现存储","type":"task","priority":"medium"}
        ]"#;
        let items = parse_breakdown_items(reply);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].subject, "设计接口");
        assert_eq!(items[0].description.as_deref(), Some("定义 REST"));
        assert_eq!(items[0].kind, "story");
        assert_eq!(items[0].priority, "high");
        assert_eq!(items[1].description, None);
        assert_eq!(items[1].kind, "task");
    }

    #[test]
    fn strips_prose_and_fences() {
        let reply = "好的，拆解如下：\n```json\n[{\"subject\":\"任务A\"}]\n```\n以上。";
        let items = parse_breakdown_items(reply);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].subject, "任务A");
        // Missing fields fall back to defaults.
        assert_eq!(items[0].kind, "story");
        assert_eq!(items[0].priority, "medium");
    }

    #[test]
    fn clamps_invalid_enums_and_skips_empty_subject() {
        let reply = r#"[
            {"subject":"  ","type":"story"},
            {"subject":"有效","type":"nonsense","priority":"crazy"}
        ]"#;
        let items = parse_breakdown_items(reply);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].subject, "有效");
        assert_eq!(items[0].kind, "story");
        assert_eq!(items[0].priority, "medium");
    }

    #[test]
    fn empty_on_unparseable() {
        assert!(parse_breakdown_items("抱歉我无法拆解").is_empty());
        assert!(parse_breakdown_items("").is_empty());
        assert!(parse_breakdown_items("[not json]").is_empty());
    }

    #[test]
    fn caps_at_max_children() {
        let mut arr = String::from("[");
        for i in 0..40 {
            if i > 0 {
                arr.push(',');
            }
            arr.push_str(&format!("{{\"subject\":\"item{i}\"}}"));
        }
        arr.push(']');
        assert_eq!(parse_breakdown_items(&arr).len(), MAX_CHILDREN);
    }
}
