#![allow(dead_code)]

pub(crate) mod agent_resolver;
pub(crate) mod control;
pub(crate) mod plan_mode;
pub(crate) mod prompt;
pub(crate) mod registry;
pub(crate) mod status;
pub(crate) mod subagent_result;

pub use control::AgentControl;
pub(crate) use control::{AGENT_MAX_DEPTH, InlineChildCompletionReceiver};
pub(crate) use plan_mode::PLAN_MODE_INSTRUCTIONS;
pub use prompt::SystemPromptKind;
