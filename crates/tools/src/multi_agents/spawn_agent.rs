use rig::{completion::ToolDefinition, tool::Tool};
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};

use crate::{
    ToolFailure,
    multi_agents::client::{AgentInfo, DynMultiAgentClient, SpawnAgentParams},
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

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SpawnAgentOutput {
    pub thread_id: String,
    pub agent_path: String,
    pub agent_nickname: Option<String>,
    pub agent_role: Option<String>,
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
        let AgentInfo {
            thread_id,
            agent_path,
            agent_nickname,
            agent_role,
            ..
        } = self
            .client
            .spawn(SpawnAgentParams {
                message: args.message,
                agent_type: args.agent_type,
                model: args.model,
                fork_context: args.fork_context,
            })
            .await?;
        serde_json::to_string(&SpawnAgentOutput {
            thread_id,
            agent_path,
            agent_nickname,
            agent_role,
        })
        .map_err(|err| ToolFailure::new(format!("failed to encode spawn_agent output: {err}")))
    }
}
