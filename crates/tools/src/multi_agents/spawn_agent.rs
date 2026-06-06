use rig::{completion::ToolDefinition, tool::Tool};
use schemars::{JsonSchema, schema_for};
use serde::Deserialize;

use crate::ToolError;

#[derive(Clone)]
pub struct SpawnAgentTool {
    description: String,
}

#[derive(Clone)]
pub struct ExploreTool {
    description: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SubagentArgs {
    /// The task for the sub-agent to perform.
    pub instruction: String,
    /// Seed the child with the parent thread's persisted context.
    #[serde(default)]
    pub fork_context: bool,
}

impl SpawnAgentTool {
    pub fn new(description: String) -> Self {
        Self { description }
    }
}

impl ExploreTool {
    pub fn new(description: String) -> Self {
        Self { description }
    }
}

impl Tool for SpawnAgentTool {
    const NAME: &'static str = "spawn_agent";

    type Error = ToolError;
    type Args = SubagentArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: self.description.clone(),
            parameters: schema_for!(SubagentArgs).to_value(),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let _ = args;
        Err(ToolError::unsupported(
            "spawn_agent is executed by the smooth-core manual tool loop",
        ))
    }
}

impl Tool for ExploreTool {
    const NAME: &'static str = "explore";

    type Error = ToolError;
    type Args = SubagentArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: self.description.clone(),
            parameters: schema_for!(SubagentArgs).to_value(),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let _ = args;
        Err(ToolError::unsupported(
            "explore is executed by the smooth-core manual tool loop",
        ))
    }
}
