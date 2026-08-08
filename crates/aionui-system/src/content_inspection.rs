//! Local content inspection (T4): the member-side half of company DLP.
//!
//! # Why the check runs here and not on the server
//!
//! The text to inspect is the prompt, and the prompt is assembled on the
//! member's own co-located backend. Inspecting it server-side would mean either
//! a round trip on every single send — latency on the hottest path in the
//! product, and a policy that fails open the moment the company server is
//! unreachable — or inspecting only traffic that happens to go through the
//! company model proxy, which misses the case worth catching most: an employee
//! pasting a contract into a model they configured themselves.
//!
//! So rules come *down* and findings go *up*, and the decision is made locally.
//!
//! # How rules get here
//!
//! Exactly the way managed model channels do (`managed_provider.rs`): the
//! renderer holds the enterprise session, fetches from whichever backend is
//! authoritative for governance, and pushes the result into this local backend.
//! One mechanism covers standalone, server-host and client deployments, because
//! the renderer's governance routing already knows which is which.
//!
//! # What this deliberately is not
//!
//! Not a defence against a member who controls their own machine. Someone
//! determined can stop the sync, patch the binary, or open a browser. This
//! catches the accident — the paste that nobody meant as exfiltration — and
//! gives the company a record of it. Treating it as a hard security boundary
//! would be claiming a guarantee the architecture cannot make.

use std::collections::VecDeque;
use std::sync::{Mutex, RwLock};

use aionui_common::dlp::{DlpRule, scan};
use serde::{Deserialize, Serialize};

/// How many findings we hold while waiting for the renderer to ship them up.
///
/// Bounded because the buffer lives in the process that also serves chat: a
/// member who goes offline for a week with a noisy rule must not be able to
/// grow it without limit. When it overflows we drop the *oldest* — the newest
/// finding is the one a reviewer is most likely to still be able to act on.
const MAX_PENDING_FINDINGS: usize = 500;

/// One finding, shaped for the upstream `POST /api/one/devops/dlp/events`.
///
/// Field names match `DlpEventInput` on the server. There is no value in a
/// separate wire type here — the renderer forwards this verbatim.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingFinding {
    pub conversation_id: Option<String>,
    pub model: Option<String>,
    pub rule_id: String,
    pub rule_name: String,
    pub action: String,
    pub hits: i64,
    /// Surrounding text with the matched span masked. Never the matched value —
    /// see migration 011 for why the audit trail must not become the leak.
    pub excerpt: String,
}

/// Outcome of inspecting one message.
pub struct InspectionOutcome {
    /// `Some(_)` when a rule with action `block` matched.
    pub blocked: Option<ContentBlock>,
}

/// A block, carried in parts rather than as one pre-formatted sentence.
///
/// The caller needs the rule name on its own: the HTTP layer hands it to the
/// client as a parameter so the client can say this in the reader's language.
/// A single English string would force every UI to either show English or
/// re-parse it back out.
#[derive(Debug, Clone)]
pub struct ContentBlock {
    /// The admin-authored rule name. Shown to the member — it is the only part
    /// that tells them what actually stopped the send.
    pub rule_name: String,
    /// English sentence, for clients with no translation and for logs.
    pub reason: String,
}

/// Holds the distributed rules and the findings they produce.
#[derive(Default)]
pub struct ContentInspectionService {
    rules: RwLock<Vec<DlpRule>>,
    pending: Mutex<VecDeque<PendingFinding>>,
}

impl ContentInspectionService {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the active rule set. Authoritative by design: a rule an admin
    /// deleted must stop being enforced, and merging would keep it alive
    /// forever on machines that happened to see it once.
    pub fn set_rules(&self, rules: Vec<DlpRule>) -> usize {
        let count = rules.len();
        if let Ok(mut guard) = self.rules.write() {
            *guard = rules;
        }
        count
    }

    pub fn rule_count(&self) -> usize {
        self.rules.read().map(|r| r.len()).unwrap_or(0)
    }

    /// Inspect one outgoing message.
    ///
    /// Records every finding; returns a blocking reason only if some rule is
    /// configured to block. With no rules distributed this is a read lock and a
    /// length check — the cost on personal builds is nil.
    pub fn inspect(&self, conversation_id: Option<&str>, model: Option<&str>, text: &str) -> InspectionOutcome {
        let rules = match self.rules.read() {
            Ok(guard) if !guard.is_empty() => guard.clone(),
            // A poisoned lock must not silently disable the policy, but it also
            // must not break sending. Log loudly and let the message through.
            Err(_) => {
                tracing::error!("content inspection rules lock poisoned; message not inspected");
                return InspectionOutcome { blocked: None };
            }
            _ => return InspectionOutcome { blocked: None },
        };

        let result = scan(text, &rules);
        for invalid in &result.invalid_rules {
            // The server validates patterns at authoring time, so reaching here
            // means the rule was written by an older/other server or the
            // distribution mangled it. Either way it is enforcing nothing.
            tracing::warn!(rule_id = %invalid, "distributed DLP rule could not be compiled; skipped");
        }
        if result.is_empty() {
            return InspectionOutcome { blocked: None };
        }

        let blocked = result.blocking().first().map(|f| ContentBlock {
            rule_name: f.rule_name.clone(),
            reason: format!(
                "Blocked by your company's content policy (rule: {}). Remove the flagged content and try again.",
                f.rule_name
            ),
        });

        if let Ok(mut pending) = self.pending.lock() {
            for finding in &result.findings {
                if pending.len() >= MAX_PENDING_FINDINGS {
                    pending.pop_front();
                }
                pending.push_back(PendingFinding {
                    conversation_id: conversation_id.map(str::to_owned),
                    model: model.map(str::to_owned),
                    rule_id: finding.rule_id.clone(),
                    rule_name: finding.rule_name.clone(),
                    action: finding.action.as_str().to_owned(),
                    hits: finding.hits as i64,
                    excerpt: finding.excerpt.clone(),
                });
            }
        }

        InspectionOutcome { blocked }
    }

    /// Hand the buffered findings to the caller and forget them.
    ///
    /// Drain-on-read: whoever takes them owns delivery. The alternative —
    /// keeping them until acknowledged — needs an ack protocol to avoid
    /// duplicates, and duplicated findings in a review queue are worse than a
    /// rare lost one.
    pub fn drain_findings(&self) -> Vec<PendingFinding> {
        match self.pending.lock() {
            Ok(mut pending) => pending.drain(..).collect(),
            Err(_) => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_common::dlp::{DlpAction, DlpMatcher};

    fn rule(id: &str, pattern: &str, action: DlpAction) -> DlpRule {
        DlpRule {
            id: id.to_owned(),
            name: format!("rule-{id}"),
            matcher: DlpMatcher::Keyword,
            pattern: pattern.to_owned(),
            action,
        }
    }

    #[test]
    fn with_no_rules_nothing_is_inspected_or_recorded() {
        let svc = ContentInspectionService::new();
        let out = svc.inspect(Some("c1"), Some("gpt-4"), "my id card is 110101199003078515");
        assert!(out.blocked.is_none());
        assert!(svc.drain_findings().is_empty(), "personal builds must record nothing");
    }

    #[test]
    fn a_log_rule_records_without_blocking() {
        let svc = ContentInspectionService::new();
        svc.set_rules(vec![rule("r1", "project falcon", DlpAction::Log)]);

        let out = svc.inspect(Some("c1"), Some("gpt-4"), "notes about Project Falcon timeline");
        assert!(out.blocked.is_none(), "a log rule must let the send through");

        let findings = svc.drain_findings();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].rule_id, "r1");
        assert_eq!(findings[0].conversation_id.as_deref(), Some("c1"));
        assert_eq!(findings[0].action, "log");
    }

    #[test]
    fn a_block_rule_stops_the_send_and_still_records_it() {
        let svc = ContentInspectionService::new();
        svc.set_rules(vec![rule("r2", "acme merger", DlpAction::Block)]);

        let out = svc.inspect(None, None, "draft of the ACME MERGER memo");
        let block = out.blocked.expect("a block rule must block");
        // The rule name must be available on its own, not only buried in the
        // English sentence — that is what lets the UI translate around it.
        assert_eq!(block.rule_name, "rule-r2");
        assert!(
            block.reason.contains("rule-r2"),
            "the English fallback must name the rule too: {}",
            block.reason
        );
        assert_eq!(svc.drain_findings().len(), 1, "a blocked send is still an event");
    }

    #[test]
    fn the_recorded_excerpt_never_contains_the_matched_value() {
        let svc = ContentInspectionService::new();
        svc.set_rules(vec![DlpRule {
            id: "r3".into(),
            name: "ID numbers".into(),
            matcher: DlpMatcher::Builtin,
            pattern: "cn_id_card".into(),
            action: DlpAction::Log,
        }]);

        svc.inspect(Some("c1"), None, "the customer id is 110101199003078515 please check");
        let findings = svc.drain_findings();
        assert_eq!(findings.len(), 1);
        assert!(
            !findings[0].excerpt.contains("110101199003078515"),
            "the audit trail must not become the leak: {}",
            findings[0].excerpt
        );
    }

    #[test]
    fn replacing_rules_stops_enforcing_the_removed_ones() {
        let svc = ContentInspectionService::new();
        svc.set_rules(vec![rule("r1", "secret", DlpAction::Block)]);
        assert!(svc.inspect(None, None, "the secret plan").blocked.is_some());

        svc.set_rules(vec![]);
        assert!(
            svc.inspect(None, None, "the secret plan").blocked.is_none(),
            "a deleted rule must stop being enforced, not live on locally"
        );
    }

    #[test]
    fn draining_hands_over_ownership() {
        let svc = ContentInspectionService::new();
        svc.set_rules(vec![rule("r1", "hit", DlpAction::Log)]);
        svc.inspect(None, None, "hit");
        assert_eq!(svc.drain_findings().len(), 1);
        assert!(svc.drain_findings().is_empty(), "a drained finding must not be re-sent");
    }

    #[test]
    fn the_buffer_is_bounded_and_keeps_the_newest() {
        let svc = ContentInspectionService::new();
        svc.set_rules(vec![rule("r1", "hit", DlpAction::Log)]);
        for i in 0..(MAX_PENDING_FINDINGS + 10) {
            svc.inspect(Some(&format!("c{i}")), None, "hit");
        }
        let findings = svc.drain_findings();
        assert_eq!(findings.len(), MAX_PENDING_FINDINGS);
        assert_eq!(
            findings.last().unwrap().conversation_id.as_deref(),
            Some(format!("c{}", MAX_PENDING_FINDINGS + 9).as_str()),
            "overflow must drop the oldest, not the newest"
        );
    }
}
