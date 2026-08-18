//! Runtime capability modules shared across agent managers.
//!
//! These modules provide reusable primitives (CLI process supervision,
//! skill indexing, backend output/protocol sinks, first-message injection,
//! solo-team guide prompts) that any agent implementation can compose.

pub(crate) mod backend_output_sink;
pub(crate) mod backend_protocol_sink;
pub(crate) mod cli_process;
pub(crate) mod first_message_injector;
pub(crate) mod image_description;
pub(crate) mod image_input;
pub(crate) mod local_ocr_skill;
pub mod prompt_pipeline;
pub(crate) mod skill_manager;
pub(crate) mod vision_delegate;

pub use prompt_pipeline::{PostRecvHook, PreSendHook, PromptCtx, PromptPipeline};
pub use vision_delegate::AcpVisionPolicy;
