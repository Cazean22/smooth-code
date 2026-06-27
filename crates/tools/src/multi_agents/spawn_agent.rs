use rig::{completion::ToolDefinition, tool::Tool};
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Deserializer, de};

use crate::ToolError;

#[derive(Clone)]
pub struct SpawnAgentTool {
    description: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SubagentArgs {
    /// Short human-readable summary of the sub-agent task, shown in tool UIs.
    pub description: String,
    /// The focused task prompt to send to the sub-agent. For broad investigations,
    /// split independent areas into separate scoped prompts that request concrete evidence.
    pub prompt: String,
    /// Optional sub-agent type. Use `Explore` for read-only research and fact gathering;
    /// omit it, use `default`, or use `general-purpose` only when the child may need to edit files.
    #[serde(default, deserialize_with = "deserialize_subagent_type")]
    pub subagent_type: Option<String>,
}

impl SpawnAgentTool {
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
            "spawn_agent is executed by the cazean-core manual tool loop",
        ))
    }
}

fn deserialize_subagent_type<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<String>::deserialize(deserializer)?;
    match value.as_deref() {
        None | Some("default" | "general-purpose" | "Explore" | "explore") => Ok(value),
        Some(other) => Err(de::Error::custom(format!(
            "unsupported subagent_type `{other}`; supported types are `default`, `general-purpose`, `Explore`, and `explore`"
        ))),
    }
}
