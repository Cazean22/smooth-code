use std::{
    path::PathBuf,
    sync::{Arc, LazyLock, Mutex},
    time::Duration,
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
use smooth_protocol::{AgentStatus, EventMsg, ThreadId, TurnInterruptedEvent, TurnStartedEvent};
use tempfile::TempDir;
use tokio::sync::{Notify, Semaphore, watch};
use tools::{DynamicToolClient, SpawnAgentParams};

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
async fn multi_agent_client_round_trip_spawns_lists_closes_and_passively_notifies_parent() {
    let _cwd_guard = CWD_LOCK.lock().expect("cwd lock");
    let workspace = TempDir::new().expect("tempdir");
    let original_cwd = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(workspace.path()).expect("set cwd");

    let manager = ThreadManagerState::new(None, Some(Arc::new(AnyThreadFactory)))
        .await
        .expect("thread manager");
    let started = manager.start_thread().await.expect("start root");
    let root_id = started.thread_id;
    let mut root_events = manager.subscribe(root_id).await.expect("subscribe root");
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

    let mut saw_completion = false;
    let mut saw_notice = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let event = match tokio::time::timeout(remaining, root_events.recv()).await {
            Ok(Ok(event)) => event,
            Ok(Err(err)) => panic!("root event channel closed: {err}"),
            Err(_) => break,
        };

        match event.msg {
            EventMsg::CollabAgentCompleted(event) => {
                assert_eq!(event.parent_thread_id, root_id);
                assert_eq!(event.child_thread_id.to_string(), spawned.thread_id);
                assert_eq!(event.agent_path.as_str(), spawned.agent_path);
                assert_eq!(
                    event.status,
                    AgentStatus::Completed(Some(format!("done:{}", spawned.thread_id)))
                );
                saw_completion = true;
            }
            EventMsg::InterAgentMessage(event) => {
                if event.communication.author.as_str() == spawned.agent_path {
                    assert_eq!(event.communication.recipient.as_str(), "/root");
                    assert!(event.communication.trigger_turn);
                    assert!(event.communication.content.contains("[agent_completed]"));
                    assert!(
                        event
                            .communication
                            .content
                            .contains(&format!("agent_path={}", spawned.agent_path))
                    );
                    assert!(event.communication.content.contains("status=completed"));
                    assert!(event.communication.content.contains(&format!(
                        "last_assistant_message=done:{}",
                        spawned.thread_id
                    )));
                    saw_notice = true;
                }
            }
            _ => {}
        }

        if saw_completion && saw_notice {
            break;
        }
    }
    assert!(
        saw_completion,
        "expected child completion event on parent thread"
    );
    assert!(
        saw_notice,
        "expected passive completion notice in parent mailbox"
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

struct BlockingDriver {
    started: Arc<Notify>,
    release: Arc<Semaphore>,
    text: String,
}

impl SessionModelDriver for BlockingDriver {
    fn stream_turn(&self, prompt: Message, history: Vec<Message>) -> Result<SessionStream> {
        let _ = (prompt, history);
        let started = Arc::clone(&self.started);
        let release = Arc::clone(&self.release);
        let text = self.text.clone();
        Ok(Box::pin(async_stream::stream! {
            started.notify_one();
            let _permit = release.acquire().await.expect("release semaphore should stay open");
            yield Ok(SessionStreamEvent::StreamAssistantItem(
                SessionAssistantContent::Text(Text { text }),
            ));
            yield Ok(SessionStreamEvent::FinalResponse(FinalResponse::empty()));
        }))
    }
}

struct SequencedFactory {
    started: Arc<Notify>,
    release: Arc<Semaphore>,
    build_count: Mutex<usize>,
}

impl SessionModelFactory for SequencedFactory {
    fn build(
        &self,
        _cwd: PathBuf,
        thread_id: ThreadId,
        _dynamic_tool_client: Option<Arc<dyn DynamicToolClient>>,
        _current_turn_id: Arc<watch::Sender<Option<String>>>,
        _role_override: RoleOverride,
        _agent_control: AgentControl,
    ) -> Result<SessionModel> {
        let mut build_count = self.build_count.lock().expect("build count mutex");
        let model = if *build_count == 0 {
            SessionModel::Stub(Arc::new(BlockingDriver {
                started: Arc::clone(&self.started),
                release: Arc::clone(&self.release),
                text: format!("root:{thread_id}"),
            }))
        } else {
            SessionModel::Stub(Arc::new(StubDriver {
                text: format!("child:{thread_id}"),
            }))
        };
        *build_count += 1;
        Ok(model)
    }
}

#[tokio::test]
async fn child_completion_notice_waits_for_parent_turn_to_finish() {
    let _cwd_guard = CWD_LOCK.lock().expect("cwd lock");
    let workspace = TempDir::new().expect("tempdir");
    let original_cwd = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(workspace.path()).expect("set cwd");

    let started_signal = Arc::new(Notify::new());
    let release = Arc::new(Semaphore::new(0));
    let manager = ThreadManagerState::new(
        None,
        Some(Arc::new(SequencedFactory {
            started: Arc::clone(&started_signal),
            release: Arc::clone(&release),
            build_count: Mutex::new(0),
        })),
    )
    .await
    .expect("thread manager");
    let started = manager.start_thread().await.expect("start root");
    let root_id = started.thread_id;
    let mut root_events = manager.subscribe(root_id).await.expect("subscribe root");
    let initial_turn_id = manager
        .start_user_input(root_id, "parent still working".to_string())
        .await
        .expect("start root turn");

    let first_event = root_events.recv().await.expect("turn started");
    assert_eq!(
        first_event.msg,
        EventMsg::TurnStarted(TurnStartedEvent {
            thread_id: root_id.to_string(),
            turn_id: initial_turn_id.clone(),
        })
    );
    tokio::time::timeout(Duration::from_secs(1), started_signal.notified())
        .await
        .expect("root driver should start");

    let client = manager.multi_agent_client(root_id);
    let spawned = client
        .spawn(SpawnAgentParams {
            message: "finish quickly".to_string(),
            agent_type: Some("worker".to_string()),
            model: None,
            fork_context: false,
        })
        .await
        .expect("spawn should succeed");

    let quiet_until_release = tokio::time::Instant::now() + Duration::from_millis(150);
    while tokio::time::Instant::now() < quiet_until_release {
        let remaining = quiet_until_release.saturating_duration_since(tokio::time::Instant::now());
        let event = match tokio::time::timeout(remaining, root_events.recv()).await {
            Ok(Ok(event)) => event,
            Ok(Err(err)) => panic!("root event channel closed: {err}"),
            Err(_) => break,
        };

        match event.msg {
            EventMsg::TurnStarted(TurnStartedEvent { turn_id, .. }) => {
                assert_eq!(
                    turn_id, initial_turn_id,
                    "completion notice should not start a follow-up turn while parent is busy"
                );
            }
            EventMsg::TurnInterrupted(TurnInterruptedEvent { turn_id, .. }) => {
                panic!("parent turn should not be interrupted by child completion: {turn_id}");
            }
            EventMsg::InterAgentMessage(_) => {
                panic!("completion notice should stay queued until the parent becomes idle");
            }
            _ => {}
        }
    }

    release.add_permits(1);

    let mut saw_follow_up_turn = false;
    let mut saw_notice = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let event = match tokio::time::timeout(remaining, root_events.recv()).await {
            Ok(Ok(event)) => event,
            Ok(Err(err)) => panic!("root event channel closed: {err}"),
            Err(_) => break,
        };

        match event.msg {
            EventMsg::TurnStarted(TurnStartedEvent { turn_id, .. }) => {
                if turn_id != initial_turn_id {
                    saw_follow_up_turn = true;
                }
            }
            EventMsg::TurnInterrupted(TurnInterruptedEvent { turn_id, .. }) => {
                panic!("parent turn should not be interrupted by child completion: {turn_id}");
            }
            EventMsg::InterAgentMessage(event) => {
                if event.communication.author.as_str() == spawned.agent_path {
                    assert!(event.communication.content.contains("[agent_completed]"));
                    saw_notice = true;
                }
            }
            _ => {}
        }

        if saw_follow_up_turn && saw_notice {
            break;
        }
    }

    assert!(
        saw_follow_up_turn,
        "expected queued completion notice to start a follow-up turn after parent completion"
    );
    assert!(
        saw_notice,
        "expected queued completion notice to reach the parent mailbox after parent completion"
    );

    std::env::set_current_dir(original_cwd).expect("restore cwd");
}
