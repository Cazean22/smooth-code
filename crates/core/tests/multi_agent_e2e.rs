use std::{
    path::PathBuf,
    sync::{Arc, LazyLock, Mutex},
};

use anyhow::Result;
use futures_util::stream;
use rig::{
    agent::FinalResponse,
    message::{Message, Text},
};
use smooth_core::{
    AgentControl, RoleOverride, SessionAssistantContent, SessionModel, SessionModelDriver,
    SessionModelFactory, SessionStream, SessionStreamEvent, ThreadManagerState,
};
use smooth_protocol::{AgentStatus, ThreadId};
use tempfile::TempDir;
use tokio::sync::watch;
use tools::{DynamicToolClient, SpawnAgentParams, WaitAgentParams};

static CWD_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

struct StubDriver {
    text: String,
}

impl SessionModelDriver for StubDriver {
    fn stream_turn(&self, prompt: Message, history: Vec<Message>) -> Result<SessionStream> {
        let _ = (prompt, history);
        Ok(Box::pin(stream::iter(vec![
            Ok(SessionStreamEvent::StreamAssistantItem(
                SessionAssistantContent::Text(Text {
                    text: self.text.clone(),
                }),
            )),
            Ok(SessionStreamEvent::FinalResponse(FinalResponse::empty())),
        ])))
    }
}

struct AnyThreadFactory;

impl SessionModelFactory for AnyThreadFactory {
    fn build(
        &self,
        _cwd: PathBuf,
        thread_id: ThreadId,
        _dynamic_tool_client: Option<Arc<dyn DynamicToolClient>>,
        _current_turn_id: Arc<watch::Sender<Option<String>>>,
        _role_override: RoleOverride,
        _agent_control: AgentControl,
    ) -> Result<SessionModel> {
        Ok(SessionModel::Stub(Arc::new(StubDriver {
            text: format!("done:{thread_id}"),
        })))
    }
}

#[tokio::test]
async fn multi_agent_client_round_trip_spawns_waits_lists_and_closes_agents() {
    let _cwd_guard = CWD_LOCK.lock().expect("cwd lock");
    let workspace = TempDir::new().expect("tempdir");
    let original_cwd = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(workspace.path()).expect("set cwd");

    let manager = ThreadManagerState::new(None, Some(Arc::new(AnyThreadFactory)))
        .await
        .expect("thread manager");
    let started = manager.start_thread().await.expect("start root");
    let root_id = started.thread_id;
    let client = manager.multi_agent_client(root_id);

    let spawned = client
        .spawn(SpawnAgentParams {
            message: "inspect workspace".to_string(),
            agent_type: Some("explorer".to_string()),
            model: None,
            fork_context: false,
        })
        .await
        .expect("spawn should succeed");
    assert!(spawned.agent_path.starts_with("/root/"));
    assert_eq!(spawned.agent_role.as_deref(), Some("explorer"));

    let waited = client
        .wait_agent(WaitAgentParams {
            target: spawned.agent_path.clone(),
            timeout_ms: Some(250),
        })
        .await
        .expect("wait should succeed");
    assert_eq!(waited.target, spawned.agent_path);
    assert_eq!(waited.status, "completed");
    assert_eq!(
        waited.status_detail,
        AgentStatus::Completed(Some(format!("done:{}", waited.thread_id)))
    );
    assert_eq!(
        waited.last_assistant_message,
        Some(format!("done:{}", waited.thread_id))
    );

    let listed = client
        .list_agents(Some("/root".to_string()))
        .await
        .expect("list should succeed");
    assert_eq!(listed.len(), 2);
    assert!(
        listed
            .iter()
            .any(|agent| agent.agent_path == spawned.agent_path)
    );

    let closed = client
        .close_agent(spawned.agent_path)
        .await
        .expect("close should succeed");
    assert_eq!(closed, "shutdown");

    std::env::set_current_dir(original_cwd).expect("restore cwd");
}
