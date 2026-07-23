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
    // Try each bracket-balanced candidate in order; the first that parses to a
    // JSON array yielding at least one item wins. Trying candidates (rather
    // than a single first-`[`..last-`]` slice) is what makes stray brackets in
    // the surrounding prose harmless.
    for candidate in json_array_candidates(reply) {
        let Ok(serde_json::Value::Array(elems)) = serde_json::from_str::<serde_json::Value>(candidate) else {
            continue;
        };
        let items = collect_breakdown_items(elems);
        if !items.is_empty() {
            return items;
        }
    }
    Vec::new()
}

/// Map already-parsed JSON array elements into clamped `BreakdownItem`s.
fn collect_breakdown_items(elems: Vec<serde_json::Value>) -> Vec<BreakdownItem> {
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

/// Every bracket-balanced `[ ... ]` span in `reply`, in order of appearance.
///
/// LLM replies wrap the array in prose or ```json fences and sometimes leave
/// stray brackets in that prose (e.g. "参考[1]" or "如下：[...]"). A naive
/// first-`[`..last-`]` slice would swallow those and produce invalid JSON,
/// surfacing as a spurious "拆解失败". Here we bracket-match each `[` (honoring
/// JSON string literals so brackets inside `"..."` don't shift nesting) and let
/// the caller try each candidate.
fn json_array_candidates(reply: &str) -> Vec<&str> {
    let bytes = reply.as_bytes();
    let mut candidates = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        // `[` and `]` are ASCII, so these byte indices are char boundaries even
        // when the span contains multibyte UTF-8.
        if bytes[i] == b'['
            && let Some(end) = balanced_array_end(bytes, i)
        {
            candidates.push(&reply[i..=end]);
            i = end + 1;
            continue;
        }
        i += 1;
    }
    candidates
}

/// Given a `[` at `open`, return the byte index of its matching `]`, tracking
/// nested `[]`/`{}` and skipping brackets inside JSON string literals. Returns
/// `None` if the brackets never balance before end of input.
fn balanced_array_end(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, &c) in bytes[open..].iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_string = false;
            }
            continue;
        }
        match c {
            b'"' => in_string = true,
            b'[' | b'{' => depth += 1,
            b']' | b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + offset);
                }
            }
            _ => {}
        }
    }
    None
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
    fn ignores_stray_brackets_in_surrounding_prose() {
        // B2 regression: prose brackets before/after the real array used to be
        // swallowed by the first-`[`..last-`]` slice, yielding invalid JSON and
        // a spurious breakdown failure.
        let reply = "参考需求 [见附录] 拆解如下：\n[{\"subject\":\"任务A\",\"type\":\"task\"}]\n详见 [1] 与 [2]。";
        let items = parse_breakdown_items(reply);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].subject, "任务A");
        assert_eq!(items[0].kind, "task");
    }

    #[test]
    fn handles_brackets_inside_string_values() {
        // A `]` inside a JSON string must not be treated as the array end.
        let reply = r#"[{"subject":"支持 [数组] 语法","description":"处理 a[i] 下标","type":"story"}]"#;
        let items = parse_breakdown_items(reply);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].subject, "支持 [数组] 语法");
        assert_eq!(items[0].description.as_deref(), Some("处理 a[i] 下标"));
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
