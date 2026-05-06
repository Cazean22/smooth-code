use rig::{completion::ToolDefinition, tool::Tool};
use schemars::{JsonSchema, schema_for};
use serde::Deserialize;

use crate::{
    ToolFailure,
    multi_agents::client::{DynMultiAgentClient, SpawnAgentParams},
};

#[derive(Clone)]
pub struct SpawnAgentTool {
    client: DynMultiAgentClient,
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
    pub fn new(client: DynMultiAgentClient, description: String) -> Self {
        Self {
            client,
            description,
        }
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
        let output = self
            .client
            .spawn(SpawnAgentParams {
                message: args.message,
                agent_type: args.agent_type,
                model: args.model,
                fork_context: args.fork_context,
            })
            .await?;
        serde_json::to_string(&output)
            .map_err(|err| ToolFailure::new(format!("failed to encode spawn_agent output: {err}")))
    }
}
