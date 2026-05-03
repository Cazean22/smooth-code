use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, Weak},
};

use smooth_protocol::{AgentPath, ThreadId};

#[derive(Clone)]
pub(crate) struct AgentRegistry {
    state: Arc<Mutex<RegistryState>>,
}

#[derive(Default)]
struct RegistryState {
    agents_by_thread: HashMap<ThreadId, AgentMetadata>,
    thread_by_path: HashMap<AgentPath, ThreadId>,
    reserved_paths: HashSet<AgentPath>,
    next_fallback_name: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AgentMetadata {
    pub(crate) agent_id: Option<ThreadId>,
    pub(crate) agent_path: AgentPath,
    pub(crate) agent_nickname: Option<String>,
    pub(crate) agent_role: Option<String>,
    pub(crate) parent_thread_id: Option<ThreadId>,
    pub(crate) depth: i32,
}

pub(crate) struct SpawnReservation {
    registry: Weak<Mutex<RegistryState>>,
    reserved_path: AgentPath,
    parent_thread_id: ThreadId,
    depth: i32,
    committed: bool,
}

impl AgentRegistry {
    pub(crate) fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(RegistryState::default())),
        }
    }

    pub(crate) fn register_root_thread(
        &self,
        thread_id: ThreadId,
    ) -> Result<AgentMetadata, String> {
        let mut state = self.state.lock().expect("registry mutex should lock");
        if state.agents_by_thread.contains_key(&thread_id) {
            return state
                .agents_by_thread
                .get(&thread_id)
                .cloned()
                .ok_or_else(|| "missing root agent metadata".to_string());
        }
        if state.thread_by_path.contains_key(&AgentPath::root()) {
            return Err("root agent path already registered".to_string());
        }

        let metadata = AgentMetadata {
            agent_id: Some(thread_id),
            agent_path: AgentPath::root(),
            agent_nickname: None,
            agent_role: None,
            parent_thread_id: None,
            depth: 0,
        };
        state.agents_by_thread.insert(thread_id, metadata.clone());
        state
            .thread_by_path
            .insert(metadata.agent_path.clone(), thread_id);
        Ok(metadata)
    }

    pub(crate) fn reserve_spawn_slot(
        &self,
        parent_thread_id: ThreadId,
        max_depth: i32,
        max_threads: usize,
    ) -> Result<SpawnReservation, String> {
        let mut state = self.state.lock().expect("registry mutex should lock");
        let parent = state
            .agents_by_thread
            .get(&parent_thread_id)
            .cloned()
            .ok_or_else(|| format!("parent thread not registered: {parent_thread_id}"))?;
        let depth = parent.depth + 1;
        if depth > max_depth {
            return Err(format!("agent depth limit exceeded: {depth} > {max_depth}"));
        }
        if state.agents_by_thread.len() + state.reserved_paths.len() >= max_threads {
            return Err(format!("agent thread limit exceeded: {max_threads}"));
        }

        let reserved_path = loop {
            let candidate_name = next_agent_name(&mut state);
            let candidate_path = parent.agent_path.join(&candidate_name)?;
            if !state.thread_by_path.contains_key(&candidate_path)
                && !state.reserved_paths.contains(&candidate_path)
            {
                state.reserved_paths.insert(candidate_path.clone());
                break candidate_path;
            }
        };

        Ok(SpawnReservation {
            registry: Arc::downgrade(&self.state),
            reserved_path,
            parent_thread_id,
            depth,
            committed: false,
        })
    }

    pub(crate) fn agent_id_for_path(&self, path: &AgentPath) -> Option<ThreadId> {
        self.state
            .lock()
            .expect("registry mutex should lock")
            .thread_by_path
            .get(path)
            .copied()
    }

    pub(crate) fn agent_metadata_for_thread(&self, thread_id: ThreadId) -> Option<AgentMetadata> {
        self.state
            .lock()
            .expect("registry mutex should lock")
            .agents_by_thread
            .get(&thread_id)
            .cloned()
    }

    pub(crate) fn live_agents(&self) -> Vec<AgentMetadata> {
        let mut agents = self
            .state
            .lock()
            .expect("registry mutex should lock")
            .agents_by_thread
            .values()
            .cloned()
            .collect::<Vec<_>>();
        agents.sort_by(|left, right| left.agent_path.cmp(&right.agent_path));
        agents
    }

    pub(crate) fn next_thread_spawn_depth(&self, thread_id: ThreadId) -> Option<i32> {
        self.agent_metadata_for_thread(thread_id)
            .map(|metadata| metadata.depth + 1)
    }

    pub(crate) fn unregister_thread(&self, thread_id: ThreadId) -> Option<AgentMetadata> {
        let mut state = self.state.lock().expect("registry mutex should lock");
        let metadata = state.agents_by_thread.remove(&thread_id)?;
        state.thread_by_path.remove(&metadata.agent_path);
        Some(metadata)
    }
}

impl SpawnReservation {
    pub(crate) fn agent_path(&self) -> &AgentPath {
        &self.reserved_path
    }

    pub(crate) fn depth(&self) -> i32 {
        self.depth
    }

    pub(crate) fn commit(mut self, mut metadata: AgentMetadata) -> Result<AgentMetadata, String> {
        let Some(agent_id) = metadata.agent_id else {
            return Err("agent metadata must include an agent_id before commit".to_string());
        };
        let Some(registry) = self.registry.upgrade() else {
            return Err("agent registry no longer exists".to_string());
        };
        let mut state = registry.lock().expect("registry mutex should lock");
        state.reserved_paths.remove(&self.reserved_path);
        metadata.agent_path = self.reserved_path.clone();
        metadata.parent_thread_id = Some(self.parent_thread_id);
        metadata.depth = self.depth;
        state
            .thread_by_path
            .insert(metadata.agent_path.clone(), agent_id);
        state.agents_by_thread.insert(agent_id, metadata.clone());
        self.committed = true;
        Ok(metadata)
    }
}

impl Drop for SpawnReservation {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let Some(registry) = self.registry.upgrade() else {
            return;
        };
        registry
            .lock()
            .expect("registry mutex should lock")
            .reserved_paths
            .remove(&self.reserved_path);
    }
}

fn next_agent_name(state: &mut RegistryState) -> String {
    let configured = include_str!("agent_names.txt")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    let index = state.next_fallback_name;
    state.next_fallback_name += 1;
    if let Some(name) = configured.get(index % configured.len()) {
        return format!("{name}_{index}");
    }
    format!("agent_{index}")
}

#[cfg(test)]
mod tests {
    use smooth_protocol::{AgentPath, ThreadId};

    use super::{AgentMetadata, AgentRegistry};

    #[test]
    fn register_root_and_reserve_child() {
        let registry = AgentRegistry::new();
        let root_id = ThreadId::new();
        let root = registry
            .register_root_thread(root_id)
            .expect("root registration");
        assert_eq!(root.agent_path, AgentPath::root());

        let reservation = registry
            .reserve_spawn_slot(root_id, 8, 16)
            .expect("reservation");
        assert!(reservation.agent_path().as_str().starts_with("/root/"));
        assert_eq!(reservation.depth(), 1);
    }

    #[test]
    fn commit_makes_agent_live_and_resolvable() {
        let registry = AgentRegistry::new();
        let root_id = ThreadId::new();
        registry
            .register_root_thread(root_id)
            .expect("root registration");
        let reservation = registry
            .reserve_spawn_slot(root_id, 8, 16)
            .expect("reservation");
        let child_id = ThreadId::new();
        let path = reservation.agent_path().clone();
        reservation
            .commit(AgentMetadata {
                agent_id: Some(child_id),
                agent_path: AgentPath::root(),
                agent_nickname: Some("alpha".to_string()),
                agent_role: Some("worker".to_string()),
                parent_thread_id: None,
                depth: 0,
            })
            .expect("commit");

        assert_eq!(registry.agent_id_for_path(&path), Some(child_id));
        assert_eq!(registry.next_thread_spawn_depth(child_id), Some(2));
        assert_eq!(registry.live_agents().len(), 2);
    }

    #[test]
    fn reservation_drop_releases_slot() {
        let registry = AgentRegistry::new();
        let root_id = ThreadId::new();
        registry
            .register_root_thread(root_id)
            .expect("root registration");
        let first_path = registry
            .reserve_spawn_slot(root_id, 8, 16)
            .expect("reservation")
            .agent_path()
            .clone();
        let second_path = registry
            .reserve_spawn_slot(root_id, 8, 16)
            .expect("reservation")
            .agent_path()
            .clone();

        assert_ne!(first_path, second_path);
    }
}
