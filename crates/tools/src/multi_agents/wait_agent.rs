use rig::{completion::ToolDefinition, tool::Tool};
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};

use crate::{
    ToolFailure,
    multi_agents::client::{DynMultiAgentClient, WaitAgentParams},
};

#[derive(Clone)]
pub struct WaitAgentTool {
    client: DynMultiAgentClient,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WaitAgentArgs {
    pub target: String,
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WaitAgentOutput {
    pub target: String,
    pub status: String,
}

impl WaitAgentTool {
    pub fn new(client: DynMultiAgentClient) -> Self {
        Self { client }
    }
}

impl Tool for WaitAgentTool {
    const NAME: &'static str = "wait_agent";

    type Error = ToolFailure;
    type Args = WaitAgentArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Wait for a live agent to reach a terminal status.".to_string(),
            parameters: schema_for!(WaitAgentArgs).to_value(),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let result = self
            .client
            .wait_agent(WaitAgentParams {
                target: args.target,
                timeout_ms: args.timeout_ms,
            })
            .await?;
        serde_json::to_string(&WaitAgentOutput {
            target: result.target,
            status: result.status,
        })
        .map_err(|err| ToolFailure::new(format!("failed to encode wait_agent output: {err}")))
    }
}
