#![deny(clippy::unwrap_used, clippy::expect_used)]

mod ask_user_question;
mod client;
mod delete;
mod edit;
mod error;
mod exit_plan_mode;
mod multi_agents;
mod output;
mod plan_write;
mod read;
mod run_command;
mod shared;
mod write;

pub use ask_user_question::{
    AskUserQuestionArgs, AskUserQuestionInput, AskUserQuestionOptionInput, AskUserQuestionTool,
};
pub use client::AskUserClient;
pub use delete::{DeleteArgs, DeleteTool};
pub use edit::{EditArgs, EditTool};
pub use error::{ToolError, ToolResult};
pub use exit_plan_mode::{ExitPlanModeArgs, ExitPlanModeTool};
pub use multi_agents::{SpawnAgentTool, SubagentArgs};
pub use output::{
    DecodedToolOutput, MAX_FILE_CHANGE_BYTES, decode_tool_output_for_tool, encode_tool_output,
    encode_tool_output_with_file_changes,
};
pub use plan_write::{PlanWriteArgs, PlanWriteTool};
pub use read::{ReadArgs, ReadTool};
pub use run_command::{RunCommandArgs, RunCommandTool};
pub use write::{WriteArgs, WriteTool};
