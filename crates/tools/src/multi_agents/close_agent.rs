use rig::{completion::ToolDefinition, tool::Tool};
use schemars::{JsonSchema, schema_for};
use serde::Deserialize;

use crate::{ToolFailure, multi_agents::client::DynMultiAgentClient};

#[derive(Clone)]
pub struct CloseAgentTool {
    client: DynMultiAgentClient,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CloseAgentArgs {
    pub target: String,
}

impl CloseAgentTool {
    pub fn new(client: DynMultiAgentClient) -> Self {
        Self { client }
    }
}

impl Tool for CloseAgentTool {
    const NAME: &'static str = "close_agent";

    type Error = ToolFailure;
    type Args = CloseAgentArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Close a live agent by thread id or agent path.".to_string(),
            parameters: schema_for!(CloseAgentArgs).to_value(),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        self.client.close_agent(args.target).await
    }
}
