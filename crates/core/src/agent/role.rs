#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RoleOverride {
    pub preamble: Option<String>,
    pub model: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RoleConfig {
    pub(crate) name: &'static str,
    pub(crate) description: &'static str,
    pub(crate) override_config: RoleOverride,
}

pub(crate) fn resolve_role(role: &str) -> Option<RoleConfig> {
    match role {
        "default" => Some(RoleConfig {
            name: "default",
            description: "General-purpose agent for coding and task execution.",
            override_config: RoleOverride::default(),
        }),
        "explorer" => Some(RoleConfig {
            name: "explorer",
            description: "Read-focused agent for investigation and concise findings.",
            override_config: RoleOverride {
                preamble: Some(
                    "You are an explorer agent. Focus on codebase investigation and concise findings."
                        .to_string(),
                ),
                model: None,
            },
        }),
        "worker" => Some(RoleConfig {
            name: "worker",
            description: "Execution-focused agent for bounded implementation work.",
            override_config: RoleOverride {
                preamble: Some(
                    "You are a worker agent. Implement the assigned bounded change directly."
                        .to_string(),
                ),
                model: None,
            },
        }),
        _ => None,
    }
}

pub(crate) fn render_spawn_agent_tool_description() -> String {
    let roles = ["default", "explorer", "worker"]
        .into_iter()
        .filter_map(resolve_role)
        .map(|role| format!("`{}`: {}", role.name, role.description))
        .collect::<Vec<_>>()
        .join("\n");
    format!("Built-in agent roles:\n{roles}")
}

#[cfg(test)]
mod tests {
    use super::{render_spawn_agent_tool_description, resolve_role};

    #[test]
    fn resolves_builtin_roles() {
        assert!(resolve_role("default").is_some());
        assert!(resolve_role("explorer").is_some());
        assert!(resolve_role("worker").is_some());
        assert!(resolve_role("missing").is_none());
    }

    #[test]
    fn renders_role_description() {
        let rendered = render_spawn_agent_tool_description();
        assert!(rendered.contains("`default`"));
        assert!(rendered.contains("`explorer`"));
        assert!(rendered.contains("`worker`"));
    }
}
