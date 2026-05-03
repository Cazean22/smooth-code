use smooth_protocol::{AgentPath, SessionSource, ThreadId};

use crate::agent::registry::{AgentMetadata, AgentRegistry};

pub(crate) fn resolve_agent_reference(
    registry: &AgentRegistry,
    session_source: &SessionSource,
    target: &str,
) -> Result<ThreadId, String> {
    if let Ok(thread_id) = target.parse::<ThreadId>() {
        return Ok(thread_id);
    }

    let base_path = session_source
        .get_agent_path()
        .unwrap_or_else(AgentPath::root);
    let resolved_path = if target.starts_with('/') {
        AgentPath::try_from(target)?
    } else {
        base_path.resolve(target)?
    };

    registry
        .agent_id_for_path(&resolved_path)
        .ok_or_else(|| format!("live agent path not found: {resolved_path}"))
}

pub(crate) fn list_agents(
    registry: &AgentRegistry,
    session_source: &SessionSource,
    path_prefix: Option<&str>,
) -> Result<Vec<AgentMetadata>, String> {
    let agents = registry.live_agents();
    let Some(path_prefix) = path_prefix else {
        return Ok(agents);
    };
    if path_prefix.is_empty() {
        return Err("path_prefix must not be empty".to_string());
    }

    let base_path = session_source
        .get_agent_path()
        .unwrap_or_else(AgentPath::root);
    let resolved_prefix = if path_prefix.starts_with('/') {
        AgentPath::try_from(path_prefix)?
    } else {
        base_path.resolve(path_prefix)?
    };

    Ok(agents
        .into_iter()
        .filter(|agent| {
            agent.agent_path == resolved_prefix
                || agent
                    .agent_path
                    .as_str()
                    .starts_with(&format!("{resolved_prefix}/"))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use smooth_protocol::{AgentPath, SessionSource, SubAgentSource, ThreadId};

    use crate::agent::registry::{AgentMetadata, AgentRegistry};

    use super::{list_agents, resolve_agent_reference};

    fn seed_registry() -> (AgentRegistry, ThreadId, ThreadId) {
        let registry = AgentRegistry::new();
        let root_id = ThreadId::new();
        registry
            .register_root_thread(root_id)
            .expect("root registration");
        let reservation = registry
            .reserve_spawn_slot(root_id, 8, 16)
            .expect("child reservation");
        let child_path = reservation.agent_path().clone();
        let child_id = ThreadId::new();
        reservation
            .commit(AgentMetadata {
                agent_id: Some(child_id),
                agent_path: AgentPath::root(),
                agent_nickname: Some("alpha".to_string()),
                agent_role: Some("worker".to_string()),
                parent_thread_id: None,
                depth: 0,
            })
            .expect("commit child");
        assert!(child_path.as_str().starts_with("/root/"));
        (registry, root_id, child_id)
    }

    #[test]
    fn resolves_thread_id_or_agent_path() {
        let (registry, root_id, child_id) = seed_registry();
        let source = SessionSource::Cli;

        assert_eq!(
            resolve_agent_reference(&registry, &source, &child_id.to_string()),
            Ok(child_id)
        );

        let child = registry
            .agent_metadata_for_thread(child_id)
            .expect("child metadata");
        assert_eq!(
            resolve_agent_reference(&registry, &source, child.agent_path.as_str()),
            Ok(child_id)
        );
        assert_ne!(root_id, child_id);
    }

    #[test]
    fn resolves_relative_paths_from_current_agent() {
        let (registry, root_id, child_id) = seed_registry();
        let child = registry
            .agent_metadata_for_thread(child_id)
            .expect("child metadata");
        let source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: root_id,
            depth: 1,
            agent_path: Some(child.agent_path.clone()),
            agent_nickname: child.agent_nickname.clone(),
            agent_role: child.agent_role.clone(),
        });

        assert_eq!(
            list_agents(&registry, &source, Some("/root"))
                .expect("list")
                .len(),
            2
        );
        assert!(resolve_agent_reference(&registry, &source, "missing").is_err());
    }

    #[test]
    fn empty_prefix_is_invalid() {
        let (registry, _, _) = seed_registry();
        assert!(list_agents(&registry, &SessionSource::Cli, Some("")).is_err());
    }
}
