#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SystemPromptKind {
    #[default]
    Root,
    DefaultSubagent,
    Explore,
}

impl SystemPromptKind {
    pub(crate) fn storage_key(self) -> &'static str {
        match self {
            Self::Root => "root",
            Self::DefaultSubagent => "default_subagent",
            Self::Explore => "explore",
        }
    }

    pub(crate) fn from_storage_key(value: &str) -> Option<Self> {
        match value {
            "root" => Some(Self::Root),
            "default_subagent" => Some(Self::DefaultSubagent),
            "explore" => Some(Self::Explore),
            _ => None,
        }
    }

    pub(crate) fn from_child_storage_key(value: Option<&str>) -> Self {
        value
            .and_then(Self::from_storage_key)
            .filter(|kind| !matches!(kind, Self::Root))
            .unwrap_or(Self::DefaultSubagent)
    }
}

pub(crate) const ROOT_SYSTEM_PROMPT: &str = include_str!("../../../../docs/system_prompt.md");

pub(crate) const DEFAULT_SUBAGENT_SYSTEM_PROMPT: &str = r#"# Smooth Code Default Subagent Prompt

You are a Smooth Code subagent working on a delegated task from a parent agent. Focus on the supplied instruction, use the repository context available to you, and report your result as a normal assistant message.

You may inspect and modify files when the task requires implementation. Keep changes scoped to the delegated instruction and follow the same repository rules as the root agent.

When your work is investigative, cite concrete files, symbols, commands, and test results. When your work changes code, verify the relevant behavior and summarize the files changed.
"#;

pub(crate) const EXPLORE_SUBAGENT_SYSTEM_PROMPT: &str =
    include_str!("../../../../docs/explore_subagent_system_prompt.md");

pub(crate) fn system_prompt_for_kind(kind: SystemPromptKind) -> &'static str {
    match kind {
        SystemPromptKind::Root => ROOT_SYSTEM_PROMPT,
        SystemPromptKind::DefaultSubagent => DEFAULT_SUBAGENT_SYSTEM_PROMPT,
        SystemPromptKind::Explore => EXPLORE_SUBAGENT_SYSTEM_PROMPT,
    }
}

pub(crate) fn render_spawn_agent_tool_description() -> String {
    "Spawn a default Smooth Code sub-agent with a built-in implementation-capable system prompt. \
Spawned children run concurrently. In mixed tool batches, `spawn_agent` may return a live JSON \
result with `event=\"agent_status\"` and a pending/running status; this means the child is still \
working, so do not produce a final answer or guess from that status. No wait tool is needed: wait \
for a later user message with `event=\"agent_completed\"` and the same `thread_id`, then use that \
result. Pure `spawn_agent` batches wait for final child results before continuing."
        .to_string()
}

pub(crate) fn render_explore_tool_description() -> String {
    "Spawn a read-only explorer sub-agent with a built-in investigation system prompt. Use it for \
codebase exploration, fact-finding, and concise findings. The child reports results as a normal \
assistant message and does not write findings to files."
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::{
        EXPLORE_SUBAGENT_SYSTEM_PROMPT, SystemPromptKind, render_explore_tool_description,
        render_spawn_agent_tool_description,
    };

    #[test]
    fn prompt_kind_storage_keys_round_trip() {
        for kind in [
            SystemPromptKind::Root,
            SystemPromptKind::DefaultSubagent,
            SystemPromptKind::Explore,
        ] {
            assert_eq!(
                SystemPromptKind::from_storage_key(kind.storage_key()),
                Some(kind)
            );
        }
        assert_eq!(
            SystemPromptKind::from_child_storage_key(Some("missing")),
            SystemPromptKind::DefaultSubagent
        );
        assert_eq!(
            SystemPromptKind::from_child_storage_key(Some("root")),
            SystemPromptKind::DefaultSubagent
        );
    }

    #[test]
    fn explorer_prompt_is_documented_and_read_only() {
        assert!(EXPLORE_SUBAGENT_SYSTEM_PROMPT.contains("read-only shell inspection"));
        assert!(EXPLORE_SUBAGENT_SYSTEM_PROMPT.contains("Do not create, edit, delete"));
        assert!(
            EXPLORE_SUBAGENT_SYSTEM_PROMPT
                .contains("Return findings in your final assistant message")
        );
    }

    #[test]
    fn tool_descriptions_cover_wait_semantics() {
        let spawn = render_spawn_agent_tool_description();
        assert!(spawn.contains("run concurrently"));
        assert!(spawn.contains("No wait tool is needed"));
        assert!(spawn.contains("event=\"agent_completed\""));

        let explore = render_explore_tool_description();
        assert!(explore.contains("read-only explorer"));
        assert!(explore.contains("normal assistant message"));
    }
}
