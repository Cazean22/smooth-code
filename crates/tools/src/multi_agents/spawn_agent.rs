use rig::{completion::ToolDefinition, tool::Tool};
use schemars::{JsonSchema, schema_for};
use serde::Deserialize;

use crate::ToolFailure;

#[derive(Clone)]
pub struct SpawnAgentTool {
    description: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SpawnAgentArgs {
    pub message: String,
    pub agent_type: Option<String>,
    pub model: Option<String>,
    #[serde(default)]
    pub fork_context: bool,
}

impl SpawnAgentTool {
    pub fn new(description: String) -> Self {
        Self { description }
    }
}

impl Tool for SpawnAgentTool {
    const NAME: &'static str = "spawn_agent";

    type Error = ToolFailure;
    type Args = SpawnAgentArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: self.description.clone(),
            parameters: schema_for!(SpawnAgentArgs).to_value(),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let _ = args;
        Err(ToolFailure::new(
            "spawn_agent is executed by the smooth-core manual tool loop",
        ))
    }
}
