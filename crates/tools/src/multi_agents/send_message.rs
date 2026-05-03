use rig::{completion::ToolDefinition, tool::Tool};
use schemars::{JsonSchema, schema_for};
use serde::Deserialize;

use crate::{ToolFailure, multi_agents::client::DynMultiAgentClient};

#[derive(Clone)]
pub struct SendMessageTool {
    client: DynMultiAgentClient,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SendMessageArgs {
    pub target: String,
    pub content: String,
    #[serde(default)]
    pub trigger_turn: bool,
}

impl SendMessageTool {
    pub fn new(client: DynMultiAgentClient) -> Self {
        Self { client }
    }
}

impl Tool for SendMessageTool {
    const NAME: &'static str = "send_message";

    type Error = ToolFailure;
    type Args = SendMessageArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Send a message to a live agent by thread id or agent path.".to_string(),
            parameters: schema_for!(SendMessageArgs).to_value(),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        self.client
            .send_message(args.target, args.content, args.trigger_turn)
            .await
    }
}
