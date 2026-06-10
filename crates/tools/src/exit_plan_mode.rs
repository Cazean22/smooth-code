use rig::{completion::ToolDefinition, tool::Tool};
use schemars::{JsonSchema, schema_for};
use serde::Deserialize;

use crate::ToolError;

const DESCRIPTION: &str = r#"Submit your plan for user approval.

Usage:
- Call this tool from inside plan mode after writing your plan with `plan_write`; the latest plan file content is presented to the user for review.
- If the user approves, plan mode turns off and you implement the plan with the full tool set.
- If the user rejects, you stay in plan mode; revise the plan per their feedback with `plan_write`, then call this tool again.
- Optionally pass a short `reason` describing why you are exiting (e.g., "plan ready"); this is for the transcript only."#;

#[derive(Clone, Default)]
pub struct ExitPlanModeTool;

impl ExitPlanModeTool {
    pub fn new() -> Self {
        Self
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExitPlanModeArgs {
    /// Optional short note about why you are exiting plan mode (free-form).
    #[serde(default)]
    pub reason: Option<String>,
}

impl Tool for ExitPlanModeTool {
    const NAME: &'static str = "exit_plan_mode";

    type Error = ToolError;
    type Args = ExitPlanModeArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: DESCRIPTION.to_string(),
            parameters: schema_for!(ExitPlanModeArgs).to_value(),
        }
    }

    async fn call(&self, _args: Self::Args) -> Result<Self::Output, Self::Error> {
        Err(ToolError::unsupported(
            "exit_plan_mode must be intercepted by the core tool loop",
        ))
    }
}
