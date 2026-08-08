use std::sync::Arc;

use crate::service::ConversationService;
use aionui_ai_agent::{ActiveLeaseRegistry, IWorkerTaskManager};

/// Records one metered turn for the billing/usage plane (P0-3). Fire-and-forget:
/// implementations MUST NOT block or fail the send path — they spawn their own
/// async work. Wired to one-billing in aionui-app; `None` in personal builds.
pub trait UsageRecorder: Send + Sync {
    fn record_turn(&self, user_id: String, conversation_id: String);
}

/// Why the product refused an action, in a shape the HTTP layer can hand to a
/// client that wants to say it in the reader's own language.
///
/// Three parts on purpose:
/// - `code` — stable, machine-readable; what a client branches on. Never
///   derived from the message text, which is prose and gets reworded.
/// - `message` — English, for clients with no translation and for the log.
/// - `details` — the parameters a translated sentence needs (which rule, which
///   model). Without them a client can only show the English or nothing.
///
/// ⚠️ Everything in `message` reaches the user. Only put text here that was
/// written for them — never an internal error's `to_string()`.
#[derive(Debug, Clone)]
pub struct PolicyDenial {
    pub code: &'static str,
    pub message: String,
    pub details: Option<serde_json::Value>,
}

impl PolicyDenial {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            details: None,
        }
    }

    pub fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = Some(details);
        self
    }
}

/// Pre-send policy gate (P1-2 model control). Returns `Err(denial)` to BLOCK the
/// send — the team is over its spend budget, or the model is off its allowlist.
/// `None` gate, or an `Ok` result, lets the send proceed. Personal / no-company
/// users always pass. Wired to one-billing in aionui-app.
#[async_trait::async_trait]
pub trait SendGate: Send + Sync {
    async fn check_send(&self, user_id: &str, model: Option<&str>) -> Result<(), PolicyDenial>;
    /// Allowlist-only check at model-switch time (budget is enforced at send).
    async fn check_model(&self, user_id: &str, model: &str) -> Result<(), PolicyDenial>;
}

/// Inspects outgoing message text against the company's content rules (T4).
///
/// Returns `Some(reason)` to BLOCK the send. Findings are recorded by the
/// implementation regardless of the return value — a rule set to record-only
/// returns `None` and still leaves a trail. `None` inspector, or a personal
/// build with no rules distributed, always passes. Wired to aionui-system in
/// aionui-app.
///
/// Synchronous on purpose: the check is an in-memory scan on the hottest path
/// in the product, and making it awaitable would invite an implementation that
/// does I/O there.
pub trait ContentInspector: Send + Sync {
    fn inspect(&self, conversation_id: &str, text: &str) -> Option<PolicyDenial>;
}

/// Shared state for conversation route handlers.
#[derive(Clone)]
pub struct ConversationRouterState {
    pub service: ConversationService,
    pub task_manager: Arc<dyn IWorkerTaskManager>,
    pub active_leases: Arc<ActiveLeaseRegistry>,
    /// Optional usage meter; when set, each accepted send records a turn.
    pub usage_recorder: Option<Arc<dyn UsageRecorder>>,
    /// Optional pre-send policy gate (P1-2); when set, a send may be blocked.
    pub send_gate: Option<Arc<dyn SendGate>>,
    /// Optional content inspector (T4); when set, a send may be blocked and
    /// findings recorded.
    pub content_inspector: Option<Arc<dyn ContentInspector>>,
}

impl ConversationRouterState {
    /// Attach a usage recorder (one-billing). Chainable at wire-up time.
    pub fn with_usage_recorder(mut self, recorder: Arc<dyn UsageRecorder>) -> Self {
        self.usage_recorder = Some(recorder);
        self
    }

    /// Attach a pre-send policy gate (one-billing). Chainable at wire-up time.
    pub fn with_send_gate(mut self, gate: Arc<dyn SendGate>) -> Self {
        self.send_gate = Some(gate);
        self
    }

    /// Attach a content inspector (aionui-system). Chainable at wire-up time.
    pub fn with_content_inspector(mut self, inspector: Arc<dyn ContentInspector>) -> Self {
        self.content_inspector = Some(inspector);
        self
    }
}
