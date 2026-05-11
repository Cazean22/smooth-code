mod spawn_agent;

pub use spawn_agent::SpawnAgentTool;

#[cfg(test)]
mod tests {
    use rig::tool::Tool;

    #[tokio::test]
    async fn spawn_agent_tool_definition() {
        let tool = super::SpawnAgentTool::new("spawn desc".to_string());
        let definition = tool.definition(String::new()).await;
        assert_eq!(definition.name, "spawn_agent");
        assert_eq!(definition.description, "spawn desc");
    }
}
