#![deny(clippy::unwrap_used, clippy::expect_used)]

mod ask_user_question;
mod cancel;
mod client;
mod delete;
mod edit;
mod error;
mod exit_plan_mode;
mod kill_sweep;
mod multi_agents;
mod output;
mod plan_write;
mod read;
mod run_command;
mod shared;
mod skill;
mod skills;
mod todo_write;
mod write;

pub use ask_user_question::{
    AskUserQuestionArgs, AskUserQuestionInput, AskUserQuestionOptionInput, AskUserQuestionTool,
};
pub use cancel::{tool_cancel_token, with_tool_cancel_scope};
pub use client::AskUserClient;
pub use delete::{DeleteArgs, DeleteTool};
pub use edit::{EditArgs, EditTool};
pub use error::{ToolError, ToolResult};
pub use exit_plan_mode::{ExitPlanModeArgs, ExitPlanModeTool};
pub use kill_sweep::sweep_pending_process_kills;
pub use multi_agents::{SpawnAgentTool, SubagentArgs};
pub use output::{
    DecodedToolOutput, MAX_FILE_CHANGE_BYTES, decode_tool_output_for_tool, encode_tool_output,
    encode_tool_output_with_file_changes, encode_tool_output_with_todos,
};
pub use plan_write::{PlanWriteArgs, PlanWriteTool, plan_file_path};
pub use read::{ReadArgs, ReadTool};
pub use run_command::{RunCommandArgs, RunCommandTool};
pub use skill::{SkillArgs, SkillTool};
pub use skills::{
    Skill, SkillMeta, list_skills, load_skill, loaded_skill_names_in_text, project_skills_dir,
    render_skill_invocation, skill_roots,
};
pub use todo_write::{TodoInput, TodoWriteArgs, TodoWriteTool};
pub use write::{WriteArgs, WriteTool};
