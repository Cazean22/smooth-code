use smooth_protocol::AgentStatus;

use crate::agent::registry::AgentMetadata;

pub(crate) fn render_completion_notification(
    agent: &AgentMetadata,
    status: &AgentStatus,
) -> String {
    let nickname = agent
        .agent_nickname
        .as_deref()
        .unwrap_or_else(|| agent.agent_path.name());
    format!(
        "Agent `{nickname}` at `{}` reached terminal status: {status:?}",
        agent.agent_path
    )
}
