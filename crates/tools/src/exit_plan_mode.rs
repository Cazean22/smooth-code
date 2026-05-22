use rig::{completion::ToolDefinition, tool::Tool};
use schemars::{JsonSchema, schema_for};
use serde::Deserialize;

use crate::ToolFailure;

const DESCRIPTION: &str = r#"Leave plan mode now that the plan is ready.

Usage:
- Call this tool from inside plan mode after you have written your plan with `plan_write`.
- Plan mode turns off automatically on success; subsequent turns will see the full tool set and may implement the plan.
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

    type Error = ToolFailure;
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
        Err(ToolFailure::new(
            "exit_plan_mode must be intercepted by the core tool loop",
        ))
    }
}
