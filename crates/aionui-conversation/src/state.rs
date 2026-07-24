use std::sync::Arc;

use crate::service::ConversationService;
use aionui_ai_agent::{ActiveLeaseRegistry, IWorkerTaskManager};

/// Records one metered turn for the billing/usage plane (P0-3). Fire-and-forget:
/// implementations MUST NOT block or fail the send path — they spawn their own
/// async work. Wired to one-billing in aionui-app; `None` in personal builds.
pub trait UsageRecorder: Send + Sync {
    fn record_turn(&self, user_id: String, conversation_id: String);
}

/// Shared state for conversation route handlers.
#[derive(Clone)]
pub struct ConversationRouterState {
    pub service: ConversationService,
    pub task_manager: Arc<dyn IWorkerTaskManager>,
    pub active_leases: Arc<ActiveLeaseRegistry>,
    /// Optional usage meter; when set, each accepted send records a turn.
    pub usage_recorder: Option<Arc<dyn UsageRecorder>>,
}

impl ConversationRouterState {
    /// Attach a usage recorder (one-billing). Chainable at wire-up time.
    pub fn with_usage_recorder(mut self, recorder: Arc<dyn UsageRecorder>) -> Self {
        self.usage_recorder = Some(recorder);
        self
    }
}
