//! Scanning outbound text for things that should not leave the company.
//!
//! This is the pure half of DLP: given some text and a set of rules, which rules
//! matched and where. It holds no state, touches no database, and makes no
//! policy decision — deciding what to do about a match belongs to the caller,
//! and loading the rules belongs to the enterprise layer.
//!
//! # Why the recorded excerpt masks the match
//!
//! An audit trail that quotes the ID number it caught has simply moved the leak
//! into a table that, by design, aggregates every such value in the company. So
//! a finding records the surrounding sentence with the matched span **masked**:
//! enough for a human to see what kind of thing was sent and in what context,
//! without the audit log becoming the highest-value target in the deployment.
//!
//! # Regex safety
//!
//! Admin-supplied patterns go through the `regex` crate, which has no
//! backtracking and therefore no catastrophic-blowup class — a hostile or
//! careless pattern costs linear time, not minutes of CPU. A pattern that fails
//! to compile is skipped and reported, never silently treated as "matches
//! nothing" (that would look identical to a rule that is working).

use serde::{Deserialize, Serialize};

/// How a rule decides what counts as a hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DlpMatcher {
    /// Case-insensitive substring. What most admins actually want (a customer
    /// name, a project codename) and what they can write correctly.
    Keyword,
    /// Full regular expression, for the cases a keyword cannot express.
    Regex,
    /// One of the shipped patterns, selected by id (see [`builtin_spec`]).
    Builtin,
}

/// What the company wants to happen when a rule hits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DlpAction {
    /// Record it and let the send through. The default for a new rule: a rule
    /// that blocks from day one, before anyone has seen its real hit rate, tends
    /// to produce false positives that drive people to paste into a browser
    /// instead — worse for the company than the leak it was meant to stop.
    Log,
    /// Refuse the send.
    Block,
}

impl DlpAction {
    pub fn blocks(self) -> bool {
        matches!(self, DlpAction::Block)
    }

    /// The storage/wire spelling, matching the `action` CHECK constraint on
    /// `one_dlp_rules`. Kept next to the enum so the two cannot drift.
    pub fn as_str(self) -> &'static str {
        match self {
            DlpAction::Log => "log",
            DlpAction::Block => "block",
        }
    }
}

/// One rule, as the enforcement layer sees it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DlpRule {
    pub id: String,
    pub name: String,
    pub matcher: DlpMatcher,
    /// The keyword, the regex source, or the builtin id.
    pub pattern: String,
    pub action: DlpAction,
}

/// One rule hitting one piece of text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DlpFinding {
    pub rule_id: String,
    pub rule_name: String,
    pub action: DlpAction,
    /// How many times this rule hit. Reported rather than one finding per hit:
    /// a document with forty ID numbers is one problem, not forty.
    pub hits: usize,
    /// Surrounding context with the matched span masked. See the module docs for
    /// why this is not the raw match.
    pub excerpt: String,
}

impl DlpFinding {
    pub fn blocks(&self) -> bool {
        self.action.blocks()
    }
}

/// Characters of context kept on each side of a match in the excerpt.
const EXCERPT_CONTEXT: usize = 40;

/// A shipped pattern and the checks that make it specific enough to be useful.
///
/// ⚠️ The `regex` crate has **no look-around** — that is the price of its
/// linear-time guarantee, and it is the reason digit boundaries are not baked
/// into these patterns. Writing `(?<![0-9])` here compiles nowhere; the
/// equivalent has to be checked against the surrounding text in Rust, which is
/// exact and cannot be defeated by an unlucky pattern rewrite.
pub struct BuiltinSpec {
    pub regex: &'static str,
    /// Reject a hit that is glued to another digit on either side. Without this
    /// a "mobile number" rule fires inside every long order id.
    pub digit_bounded: bool,
    /// Additionally require the digits to pass a Luhn check.
    pub luhn: bool,
}

/// Shipped patterns, so a company gets something useful without writing regexes.
///
/// Deliberately shaped to be specific rather than eager. A pattern that flags
/// every 11-digit number would fire on order ids and timestamps, and a DLP tool
/// people learn to ignore protects nothing.
pub fn builtin_spec(id: &str) -> Option<BuiltinSpec> {
    let (regex, digit_bounded, luhn) = match id {
        // Mainland China resident ID: administrative prefix + birth date + 3
        // digits + check character. The embedded date is what keeps it from
        // matching arbitrary 18-digit numbers.
        "cn_id_card" => (
            r"[1-9][0-9]{5}(?:19|20)[0-9]{2}(?:0[1-9]|1[0-2])(?:0[1-9]|[12][0-9]|3[01])[0-9]{3}[0-9Xx]",
            true,
            false,
        ),
        // Bank card: 13–19 digits. Length alone is weak, so Luhn gates it.
        "bank_card" => (r"[0-9]{13,19}", true, true),
        // Mainland China mobile number.
        "cn_mobile" => (r"1[3-9][0-9]{9}", true, false),
        // Provider API keys, by the prefixes vendors actually use.
        "api_key" => (
            r"\b(?:sk-[A-Za-z0-9_\-]{16,}|ghp_[A-Za-z0-9]{20,}|gho_[A-Za-z0-9]{20,}|AKIA[0-9A-Z]{16}|AIza[0-9A-Za-z_\-]{30,}|xox[baprs]-[A-Za-z0-9\-]{10,})",
            false,
            false,
        ),
        // Any PEM private key block header.
        "private_key" => (r"-----BEGIN (?:[A-Z ]+ )?PRIVATE KEY-----", false, false),
        _ => return None,
    };
    Some(BuiltinSpec {
        regex,
        digit_bounded,
        luhn,
    })
}

/// Every shipped pattern id, for an admin UI to offer as checkboxes.
pub const BUILTIN_PATTERN_IDS: [&str; 5] = ["cn_id_card", "bank_card", "cn_mobile", "api_key", "private_key"];

/// Is the match at `[start, end)` free of digits (and the ID card's `X`) on
/// either side? The look-behind/look-ahead the regex engine cannot express.
fn digit_boundaries_ok(text: &str, start: usize, end: usize) -> bool {
    let glued = |c: char| c.is_ascii_digit() || c == 'X' || c == 'x';
    let before_ok = text[..start].chars().next_back().is_none_or(|c| !glued(c));
    let after_ok = text[end..].chars().next().is_none_or(|c| !glued(c));
    before_ok && after_ok
}

/// Luhn check, used to keep the bank-card pattern from firing on any long digit
/// run. Without it the rule is noise, and noisy rules get switched off.
fn luhn_ok(digits: &str) -> bool {
    let ds: Vec<u32> = digits.chars().filter_map(|c| c.to_digit(10)).collect();
    if ds.len() < 13 || ds.len() > 19 {
        return false;
    }
    let sum: u32 = ds
        .iter()
        .rev()
        .enumerate()
        .map(|(i, d)| {
            if !i.is_multiple_of(2) {
                let doubled = d * 2;
                if doubled > 9 { doubled - 9 } else { doubled }
            } else {
                *d
            }
        })
        .sum();
    sum.is_multiple_of(10)
}

/// Replace a matched span with a mask that keeps its shape but not its value.
///
/// Short matches are masked whole; longer ones keep two characters at each end,
/// which is what lets a human recognise "yes, that was the customer's card"
/// without the log holding a usable number.
fn mask(matched: &str) -> String {
    let chars: Vec<char> = matched.chars().collect();
    if chars.len() <= 6 {
        return "*".repeat(chars.len());
    }
    let head: String = chars[..2].iter().collect();
    let tail: String = chars[chars.len() - 2..].iter().collect();
    format!("{head}{}{tail}", "*".repeat(chars.len() - 4))
}

/// Char-safe slice of `text` around `[start, end)` (byte offsets), with the
/// matched span masked.
fn excerpt_with_mask(text: &str, start: usize, end: usize) -> String {
    let before_start = text[..start]
        .char_indices()
        .rev()
        .take(EXCERPT_CONTEXT)
        .last()
        .map(|(i, _)| i)
        .unwrap_or(start);
    let after_end = text[end..]
        .char_indices()
        .take(EXCERPT_CONTEXT)
        .last()
        .map(|(i, c)| end + i + c.len_utf8())
        .unwrap_or(end);

    let mut out = String::new();
    if before_start > 0 {
        out.push('…');
    }
    out.push_str(&text[before_start..start]);
    out.push_str(&mask(&text[start..end]));
    out.push_str(&text[end..after_end]);
    if after_end < text.len() {
        out.push('…');
    }
    out
}

/// Outcome of scanning one piece of text.
#[derive(Debug, Clone, Default)]
pub struct DlpScan {
    pub findings: Vec<DlpFinding>,
    /// Rules whose pattern would not compile. Surfaced so a broken rule is
    /// visibly broken instead of silently permissive.
    pub invalid_rules: Vec<String>,
}

impl DlpScan {
    /// The findings that should stop the send, if any.
    pub fn blocking(&self) -> Vec<&DlpFinding> {
        self.findings.iter().filter(|f| f.blocks()).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.findings.is_empty()
    }
}

/// Scan `text` against `rules`.
///
/// Every rule is evaluated — the scan does not stop at the first blocking hit,
/// because the audit record is more useful when it lists everything that was in
/// the message rather than whichever rule happened to be ordered first.
pub fn scan(text: &str, rules: &[DlpRule]) -> DlpScan {
    let mut out = DlpScan::default();
    if text.is_empty() {
        return out;
    }

    for rule in rules {
        match rule.matcher {
            DlpMatcher::Keyword => {
                let needle = rule.pattern.trim();
                if needle.is_empty() {
                    continue;
                }
                let hay = text.to_lowercase();
                let need = needle.to_lowercase();
                let hits = hay.matches(&need).count();
                if hits > 0 {
                    // Locate the first hit in the original text for the excerpt.
                    // Lowercasing can change byte lengths, so search the original
                    // case-insensitively by scanning char boundaries.
                    let at = find_ci(text, needle).unwrap_or(0);
                    out.findings.push(DlpFinding {
                        rule_id: rule.id.clone(),
                        rule_name: rule.name.clone(),
                        action: rule.action,
                        hits,
                        excerpt: excerpt_with_mask(text, at, at + needle.len()),
                    });
                }
            }
            DlpMatcher::Regex | DlpMatcher::Builtin => {
                let (source, digit_bounded, luhn_gated) = if rule.matcher == DlpMatcher::Builtin {
                    match builtin_spec(&rule.pattern) {
                        Some(spec) => (spec.regex.to_owned(), spec.digit_bounded, spec.luhn),
                        None => {
                            out.invalid_rules.push(rule.id.clone());
                            continue;
                        }
                    }
                } else {
                    // An admin's own pattern is taken as written. Note that
                    // look-around is unsupported by this engine and such a
                    // pattern lands in `invalid_rules` rather than silently
                    // matching nothing.
                    (rule.pattern.clone(), false, false)
                };
                let re = match regex::Regex::new(&source) {
                    Ok(re) => re,
                    Err(_) => {
                        out.invalid_rules.push(rule.id.clone());
                        continue;
                    }
                };
                let mut hits = 0usize;
                let mut first: Option<(usize, usize)> = None;
                for m in re.find_iter(text) {
                    if digit_bounded && !digit_boundaries_ok(text, m.start(), m.end()) {
                        continue;
                    }
                    if luhn_gated && !luhn_ok(m.as_str()) {
                        continue;
                    }
                    hits += 1;
                    if first.is_none() {
                        first = Some((m.start(), m.end()));
                    }
                }
                if let Some((s, e)) = first {
                    out.findings.push(DlpFinding {
                        rule_id: rule.id.clone(),
                        rule_name: rule.name.clone(),
                        action: rule.action,
                        hits,
                        excerpt: excerpt_with_mask(text, s, e),
                    });
                }
            }
        }
    }
    out
}

/// Byte offset of the first case-insensitive occurrence of `needle`.
///
/// Only sound for needles whose lowercase form has the same byte length, which
/// is why the excerpt falls back to offset 0 rather than slicing blindly.
fn find_ci(haystack: &str, needle: &str) -> Option<usize> {
    let hl = haystack.to_lowercase();
    let nl = needle.to_lowercase();
    let idx = hl.find(&nl)?;
    if hl.len() == haystack.len() && nl.len() == needle.len() {
        Some(idx)
    } else {
        haystack.find(needle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(id: &str, matcher: DlpMatcher, pattern: &str, action: DlpAction) -> DlpRule {
        DlpRule {
            id: id.to_owned(),
            name: format!("rule-{id}"),
            matcher,
            pattern: pattern.to_owned(),
            action,
        }
    }

    fn builtin(id: &str, action: DlpAction) -> DlpRule {
        rule(id, DlpMatcher::Builtin, id, action)
    }

    /// The rule this module exists to protect: a finding must not carry the
    /// value it caught, or the audit table becomes the thing worth stealing.
    #[test]
    fn a_finding_never_records_the_matched_value() {
        let secret = "sk-abcdefghijklmnopqrstuvwx1234567890";
        let scan = scan(
            &format!("这是我们的密钥 {secret} 请保管好"),
            &[builtin("api_key", DlpAction::Log)],
        );
        assert_eq!(scan.findings.len(), 1);
        let excerpt = &scan.findings[0].excerpt;
        assert!(!excerpt.contains(secret), "the key leaked into the excerpt: {excerpt}");
        assert!(excerpt.contains('*'), "nothing was masked: {excerpt}");
        // …while still showing a human what the sentence was about.
        assert!(excerpt.contains("请保管好"), "context was lost: {excerpt}");
    }

    #[test]
    fn masking_keeps_enough_shape_to_recognise_the_value() {
        assert_eq!(mask("12345"), "*****");
        assert_eq!(mask("1234567890"), "12******90");
    }

    #[test]
    fn detects_a_private_key_block() {
        let scan = scan(
            "-----BEGIN RSA PRIVATE KEY-----\nMIIE...",
            &[builtin("private_key", DlpAction::Block)],
        );
        assert_eq!(scan.findings.len(), 1);
        assert!(scan.findings[0].blocks());
    }

    /// A bank-card rule that fires on any long digit run is noise, and noisy
    /// rules get switched off — so Luhn gates it.
    #[test]
    fn bank_card_rule_ignores_digit_runs_that_are_not_cards() {
        let valid = "4111111111111111"; // canonical Luhn-valid test number
        let invalid = "4111111111111112";
        let rules = [builtin("bank_card", DlpAction::Log)];

        assert_eq!(scan(&format!("卡号 {valid}"), &rules).findings.len(), 1);
        assert!(
            scan(&format!("订单号 {invalid}"), &rules).is_empty(),
            "a non-Luhn digit run was reported as a card"
        );
    }

    #[test]
    fn detects_a_mainland_id_card_but_not_any_18_digits() {
        let rules = [builtin("cn_id_card", DlpAction::Log)];
        assert_eq!(scan("身份证 11010519900307123X", &rules).findings.len(), 1);
        // Month 13 is not a date, so this is some other 18-digit number.
        assert!(scan("编号 110105199013071234", &rules).is_empty());
    }

    #[test]
    fn keyword_rules_are_case_insensitive_and_counted() {
        let scan = scan(
            "Project Bluebird is secret. bluebird again, BLUEBIRD once more.",
            &[rule("k1", DlpMatcher::Keyword, "bluebird", DlpAction::Log)],
        );
        assert_eq!(scan.findings.len(), 1, "one rule, one finding");
        assert_eq!(scan.findings[0].hits, 3, "all three spellings counted");
    }

    /// Forty ID numbers in one document is one problem to review, not forty
    /// audit rows.
    #[test]
    fn repeated_hits_collapse_into_one_finding_with_a_count() {
        let text = "11010519900307123X 11010519900307123X 11010519900307123X";
        let scan = scan(text, &[builtin("cn_id_card", DlpAction::Log)]);
        assert_eq!(scan.findings.len(), 1);
        assert_eq!(scan.findings[0].hits, 3);
    }

    /// A rule that cannot compile must be visibly broken. Treating it as
    /// "matched nothing" is indistinguishable from a rule that works, which is
    /// how a company ends up believing it is protected when it is not.
    #[test]
    fn an_uncompilable_rule_is_reported_not_silently_ignored() {
        let scan = scan(
            "anything",
            &[rule("bad", DlpMatcher::Regex, "([unclosed", DlpAction::Block)],
        );
        assert!(scan.is_empty());
        assert_eq!(scan.invalid_rules, vec!["bad".to_owned()]);
    }

    #[test]
    fn an_unknown_builtin_id_is_reported_too() {
        let scan = scan(
            "anything",
            &[rule("b", DlpMatcher::Builtin, "no_such_pattern", DlpAction::Log)],
        );
        assert_eq!(scan.invalid_rules, vec!["b".to_owned()]);
    }

    /// Every rule is evaluated, so the audit record lists everything that was in
    /// the message rather than whichever rule sorted first.
    #[test]
    fn scanning_does_not_stop_at_the_first_blocking_hit() {
        let scan = scan(
            "身份证 11010519900307123X 手机 13800138000",
            &[
                builtin("cn_id_card", DlpAction::Block),
                builtin("cn_mobile", DlpAction::Log),
            ],
        );
        assert_eq!(scan.findings.len(), 2);
        assert_eq!(scan.blocking().len(), 1);
    }

    #[test]
    fn no_rules_or_empty_text_finds_nothing() {
        assert!(scan("身份证 11010519900307123X", &[]).is_empty());
        assert!(scan("", &[builtin("cn_id_card", DlpAction::Log)]).is_empty());
    }

    /// Excerpting must not slice through a multi-byte character.
    #[test]
    fn excerpts_survive_non_ascii_context() {
        let text = "客户资料：身份证号码是 11010519900307123X，请勿外传给任何第三方合作伙伴。";
        let scan = scan(text, &[builtin("cn_id_card", DlpAction::Log)]);
        assert_eq!(scan.findings.len(), 1);
        assert!(scan.findings[0].excerpt.contains("请勿外传"));
    }

    #[test]
    fn every_shipped_pattern_compiles() {
        for id in BUILTIN_PATTERN_IDS {
            let spec = builtin_spec(id).unwrap_or_else(|| panic!("{id} missing"));
            regex::Regex::new(spec.regex).unwrap_or_else(|e| panic!("{id} does not compile: {e}"));
        }
    }

    /// The engine has no look-around, so a digit-adjacency rule has to be
    /// enforced in Rust. Pinned because the failure is silent: without it a
    /// "mobile number" rule fires inside every long order id.
    #[test]
    fn digit_bounded_patterns_do_not_fire_inside_longer_numbers() {
        let rules = [builtin("cn_mobile", DlpAction::Log)];
        assert_eq!(scan("手机 13800138000", &rules).findings.len(), 1);
        assert!(
            scan("订单 9913800138000123", &rules).is_empty(),
            "a mobile pattern fired inside a longer digit run"
        );
    }
}
