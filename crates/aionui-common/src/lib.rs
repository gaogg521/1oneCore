#![warn(clippy::disallowed_types)]

//! Shared primitives: error types, enums, ID generation, crypto, timestamps, and pagination.
pub mod constants;

mod agent_bridge;
mod case_convert;
mod crypto;
mod enums;
mod error;
mod hooks;
mod id;
pub mod license;
mod pagination;
mod timestamp;
mod tool_schema;
mod types;

pub use agent_bridge::{
    CLAUDE_BRIDGE_HOME_DIR_NAME, CLAUDE_CONFIG_DIR_ENV_KEY, claude_bridge_home, ensure_claude_bridge_home,
};
pub use case_convert::{camel_to_snake, normalize_keys_to_snake_case};
pub use crypto::{CryptoError, decrypt_string, encrypt_string};
pub use enums::{
    AgentKillReason, AgentType, ConversationSource, ConversationStatus, FileChangeOperation, McpServerStatus,
    McpSource, MessagePosition, MessageStatus, MessageType, PreviewContentType, ProtocolType, RemoteAgentAuthType,
    RemoteAgentProtocol, RemoteAgentStatus,
};
#[allow(clippy::disallowed_types)]
pub use error::{
    ApiError, ApiErrorLogContext, ErrorChain, WorkspacePathValidationError, validate_workspace_path_availability,
};
pub use hooks::OnConversationDelete;
pub use id::{fnv1a_hex8, generate_id, generate_id_with_length, generate_prefixed_id, generate_short_id};
pub use pagination::PaginatedResult;
pub use timestamp::{TimestampMs, now_ms};
pub use tool_schema::{IncompatibilityReason, SchemaIncompatibility, is_tool_compatible, validate_tool};
pub use types::{CommandSpec, Confirmation, ConfirmationOption, EnvVar, ProviderWithModel, UpdateType, VersionInfo};
