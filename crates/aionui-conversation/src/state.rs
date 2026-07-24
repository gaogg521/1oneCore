use std::sync::Arc;

use crate::service::ConversationService;
use aionui_ai_agent::{ActiveLeaseRegistry, IWorkerTaskManager};

/// Records one metered turn for the billing/usage plane (P0-3). Fire-and-forget:
/// implementations MUST NOT block or fail the send path — they spawn their own
/// async work. Wired to one-billing in aionui-app; `None` in personal builds.
pub trait UsageRecorder: Send + Sync {
    fn record_turn(&self, user_id: String, conversation_id: String);
}

/// Pre-send policy gate (P1-2 model control). Returns `Err(reason)` to BLOCK the
/// send — the team is over its spend budget, or the model is off its allowlist.
/// `None` gate, or an `Ok` result, lets the send proceed. Personal / no-company
/// users always pass. Wired to one-billing in aionui-app.
#[async_trait::async_trait]
pub trait SendGate: Send + Sync {
    async fn check_send(&self, user_id: &str, model: Option<&str>) -> Result<(), String>;
    /// Allowlist-only check at model-switch time (budget is enforced at send).
    async fn check_model(&self, user_id: &str, model: &str) -> Result<(), String>;
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
}
