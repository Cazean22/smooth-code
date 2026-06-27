mod spawn_agent;

pub use spawn_agent::{SpawnAgentTool, SubagentArgs};

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
        let parameters = definition.parameters.to_string();
        assert!(parameters.contains("description"));
        assert!(parameters.contains("prompt"));
        assert!(parameters.contains("subagent_type"));
        assert!(parameters.contains("focused task prompt"));
        assert!(parameters.contains("read-only research"));
        assert!(parameters.contains("concrete evidence"));
        assert!(!parameters.contains("\"fork_context\""));
        assert!(!parameters.contains("\"agent_type\""));
    }

    #[test]
    fn subagent_args_reject_removed_fields() {
        let old_args = json!({
            "message": "inspect",
            "agent_type": "worker",
            "agent_role": "worker",
            "model": "gpt-test",
            "system_prompt": "custom",
            "instruction": "inspect",
            "fork_context": false,
            "run_in_background": true,
            "isolation": "workspace"
        });

        assert!(serde_json::from_value::<super::SubagentArgs>(old_args).is_err());
    }

    #[test]
    fn subagent_args_accept_required_prompt_fields() -> Result<(), serde_json::Error> {
        let args = serde_json::from_value::<super::SubagentArgs>(json!({
            "description": "inspect core",
            "prompt": "inspect crates/core",
            "subagent_type": "general-purpose"
        }))?;

        assert_eq!(args.description, "inspect core");
        assert_eq!(args.prompt, "inspect crates/core");
        assert_eq!(args.subagent_type.as_deref(), Some("general-purpose"));
        Ok(())
    }

    #[test]
    fn subagent_args_accept_omitted_or_default_subagent_type() -> Result<(), serde_json::Error> {
        let omitted = serde_json::from_value::<super::SubagentArgs>(json!({
            "description": "inspect",
            "prompt": "inspect"
        }))?;
        let default = serde_json::from_value::<super::SubagentArgs>(json!({
            "description": "inspect",
            "prompt": "inspect",
            "subagent_type": "default"
        }))?;

        assert_eq!(omitted.subagent_type, None);
        assert_eq!(default.subagent_type.as_deref(), Some("default"));
        Ok(())
    }

    #[test]
    fn subagent_args_accept_explore_subagent_type() -> Result<(), serde_json::Error> {
        let canonical = serde_json::from_value::<super::SubagentArgs>(json!({
            "description": "inspect",
            "prompt": "inspect",
            "subagent_type": "Explore"
        }))?;
        let lowercase = serde_json::from_value::<super::SubagentArgs>(json!({
            "description": "inspect",
            "prompt": "inspect",
            "subagent_type": "explore"
        }))?;

        assert_eq!(canonical.subagent_type.as_deref(), Some("Explore"));
        assert_eq!(lowercase.subagent_type.as_deref(), Some("explore"));
        Ok(())
    }

    #[test]
    fn subagent_args_reject_unsupported_subagent_type() {
        let args = json!({
            "description": "inspect",
            "prompt": "inspect",
            "subagent_type": "worker"
        });

        let err = match serde_json::from_value::<super::SubagentArgs>(args) {
            Ok(_) => panic!("unsupported subagent type should fail"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("supported types"));
    }

    #[test]
    fn subagent_args_require_description_and_prompt() {
        assert!(
            serde_json::from_value::<super::SubagentArgs>(json!({
                "description": "inspect"
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<super::SubagentArgs>(json!({
                "prompt": "inspect"
            }))
            .is_err()
        );
    }
}
