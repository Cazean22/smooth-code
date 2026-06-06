mod spawn_agent;

pub use spawn_agent::{ExploreTool, SpawnAgentTool, SubagentArgs};

#[cfg(test)]
mod tests {
    use rig::tool::Tool;
    use serde_json::json;

    #[tokio::test]
    async fn spawn_agent_tool_definition() {
        let tool = super::SpawnAgentTool::new("spawn desc".to_string());
        let definition = tool.definition(String::new()).await;
        assert_eq!(definition.name, "spawn_agent");
        assert_eq!(definition.description, "spawn desc");
        assert!(definition.parameters.to_string().contains("instruction"));
        assert!(!definition.parameters.to_string().contains("agent_type"));
    }

    #[tokio::test]
    async fn explore_tool_definition() {
        let tool = super::ExploreTool::new("explore desc".to_string());
        let definition = tool.definition(String::new()).await;
        assert_eq!(definition.name, "explore");
        assert_eq!(definition.description, "explore desc");
        assert!(definition.parameters.to_string().contains("instruction"));
        assert!(!definition.parameters.to_string().contains("agent_type"));
    }

    #[test]
    fn subagent_args_reject_removed_fields() {
        let old_args = json!({
            "message": "inspect",
            "agent_type": "worker",
            "agent_role": "worker",
            "model": "gpt-test",
            "system_prompt": "custom"
        });

        assert!(serde_json::from_value::<super::SubagentArgs>(old_args).is_err());
    }

    #[test]
    fn subagent_args_accept_instruction_and_fork_context() -> Result<(), serde_json::Error> {
        let args = serde_json::from_value::<super::SubagentArgs>(json!({
            "instruction": "inspect",
            "fork_context": true
        }))?;

        assert_eq!(args.instruction, "inspect");
        assert!(args.fork_context);
        Ok(())
    }
}
