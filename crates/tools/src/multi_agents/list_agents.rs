use rig::{completion::ToolDefinition, tool::Tool};
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};

use crate::{
    ToolFailure,
    multi_agents::client::{AgentInfo, DynMultiAgentClient},
};

#[derive(Clone)]
pub struct ListAgentsTool {
    client: DynMultiAgentClient,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListAgentsArgs {
    pub path_prefix: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ListAgentsOutput {
    pub agents: Vec<AgentInfo>,
}

impl ListAgentsTool {
    pub fn new(client: DynMultiAgentClient) -> Self {
        Self { client }
    }
}

impl Tool for ListAgentsTool {
    const NAME: &'static str = "list_agents";

    type Error = ToolFailure;
    type Args = ListAgentsArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "List live agents, optionally filtered by an agent-path prefix."
                .to_string(),
            parameters: schema_for!(ListAgentsArgs).to_value(),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let agents = self.client.list_agents(args.path_prefix).await?;
        serde_json::to_string(&ListAgentsOutput { agents })
            .map_err(|err| ToolFailure::new(format!("failed to encode list_agents output: {err}")))
    }
}
