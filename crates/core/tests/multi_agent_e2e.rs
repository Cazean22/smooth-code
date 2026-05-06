use std::{
    path::PathBuf,
    sync::{Arc, LazyLock, Mutex},
    time::Duration,
};

use anyhow::Result;
use futures_util::stream;
use rig::{
    agent::FinalResponse,
    message::{Message, Text, ToolCall, ToolFunction, UserContent},
};
use smooth_core::{
    AgentControl, RoleOverride, SessionAssistantContent, SessionCompletionEvent,
    SessionCompletionStream, SessionModel, SessionModelDriver, SessionModelFactory, SessionStream,
    SessionStreamEvent, SessionTurnSummary, ThreadManagerState,
};
use smooth_protocol::{AgentStatus, EventMsg, ThreadId, TurnCompletedEvent, TurnStartedEvent};
use tempfile::TempDir;
use tokio::sync::watch;
use tools::{AgentInfo, DynamicToolClient, SpawnAgentParams};

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

struct SameTurnSpawnDriver {
    calls: Mutex<usize>,
}

impl SessionModelDriver for SameTurnSpawnDriver {
    fn stream_turn(&self, _prompt: Message, _history: Vec<Message>) -> Result<SessionStream> {
        unreachable!("manual completion stream should be used for same-turn spawn test");
    }

    fn supports_manual_tool_loop(&self) -> bool {
        true
    }

    fn stream_completion_turn(
        &self,
        prompt: Message,
        history: Vec<Message>,
    ) -> Result<SessionCompletionStream> {
        let mut calls = self.calls.lock().expect("calls mutex");
        let call_idx = *calls;
        *calls += 1;
        drop(calls);

        match call_idx {
            0 => {
                assert_eq!(first_user_text(&prompt), Some("delegate child".to_string()));
                assert!(history.is_empty());
                let tool_call = ToolCall::new(
                    "spawn-1".to_string(),
                    ToolFunction::new(
                        "spawn_agent".to_string(),
                        serde_json::json!({
                            "message": "finish quickly",
                            "agent_type": "worker",
                            "fork_context": false
                        }),
                    ),
                )
                .with_call_id("call-1".to_string());
                Ok(Box::pin(stream::iter(vec![
                    Ok(SessionCompletionEvent::AssistantItem(
                        SessionAssistantContent::ToolCall {
                            tool_call,
                            internal_call_id: "internal-call-1".to_string(),
                        },
                    )),
                    Ok(SessionCompletionEvent::Completed(SessionTurnSummary {
                        assistant_message_id: Some("assistant-tool-call".to_string()),
                        response: String::new(),
                    })),
                ])))
            }
            1 => {
                assert_eq!(history.len(), 3);
                assert_eq!(
                    first_user_text(&history[0]),
                    Some("delegate child".to_string())
                );
                let tool_result = match &history[2] {
                    Message::User { content } => content
                        .iter()
                        .find_map(|item| match item {
                            UserContent::ToolResult(tool_result) => Some(tool_result),
                            _ => None,
                        })
                        .expect("tool result content"),
                    other => panic!("expected tool result message, got {other:?}"),
                };
                let tool_result_text = tool_result
                    .content
                    .iter()
                    .find_map(|item| match item {
                        rig::message::ToolResultContent::Text(text) => Some(text.text.clone()),
                        _ => None,
                    })
                    .expect("tool result text");
                let parsed: AgentInfo =
                    serde_json::from_str(&tool_result_text).expect("spawn_agent output json");
                assert_eq!(parsed.status.as_deref(), Some("completed"));
                assert_eq!(
                    parsed.status_detail,
                    Some(AgentStatus::Completed(
                        parsed.last_assistant_message.clone()
                    ))
                );
                assert!(
                    first_user_text(&prompt)
                        .expect("inline prompt text")
                        .contains("[agent_completed]")
                );

                Ok(Box::pin(stream::iter(vec![
                    Ok(SessionCompletionEvent::AssistantItem(
                        SessionAssistantContent::Text(Text {
                            text: "parent finished".to_string(),
                        }),
                    )),
                    Ok(SessionCompletionEvent::Completed(SessionTurnSummary {
                        assistant_message_id: Some("assistant-final".to_string()),
                        response: "parent finished".to_string(),
                    })),
                ])))
            }
            other => panic!("unexpected completion turn {other}"),
        }
    }
}

struct SameTurnSpawnFactory {
    build_count: Mutex<usize>,
}

impl SessionModelFactory for SameTurnSpawnFactory {
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
            SessionModel::Stub(Arc::new(SameTurnSpawnDriver {
                calls: Mutex::new(0),
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
async fn spawn_agent_waits_inline_and_finishes_in_same_parent_turn() {
    let _cwd_guard = CWD_LOCK.lock().expect("cwd lock");
    let workspace = TempDir::new().expect("tempdir");
    let original_cwd = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(workspace.path()).expect("set cwd");

    let manager = ThreadManagerState::new(
        None,
        Some(Arc::new(SameTurnSpawnFactory {
            build_count: Mutex::new(0),
        })),
    )
    .await
    .expect("thread manager");
    let started = manager.start_thread().await.expect("start root");
    let root_id = started.thread_id;
    let mut root_events = manager.subscribe(root_id).await.expect("subscribe root");
    let initial_turn_id = manager
        .start_user_input(root_id, "delegate child".to_string())
        .await
        .expect("start root turn");

    let mut turn_started = 0;
    let mut turn_completed = 0;
    let mut inter_agent_index = None;
    let mut turn_completed_index = None;
    let mut collab_completion_index = None;
    let mut tool_completed_index = None;
    let mut event_index = 0usize;
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
                turn_started += 1;
                assert_eq!(turn_id, initial_turn_id);
            }
            EventMsg::CollabAgentCompleted(event) => {
                collab_completion_index = Some(event_index);
                assert_eq!(event.parent_thread_id, root_id);
                assert_eq!(
                    event.status,
                    AgentStatus::Completed(event.last_assistant_message.clone())
                );
            }
            EventMsg::ToolCallCompleted(event) => {
                if event.call_id == "internal-call-1" {
                    tool_completed_index = Some(event_index);
                    let parsed: AgentInfo = serde_json::from_str(
                        event
                            .output_preview
                            .as_deref()
                            .expect("spawn_agent output preview"),
                    )
                    .expect("spawn_agent result json");
                    assert_eq!(parsed.status.as_deref(), Some("completed"));
                    assert_eq!(
                        parsed.status_detail,
                        Some(AgentStatus::Completed(
                            parsed.last_assistant_message.clone()
                        ))
                    );
                }
            }
            EventMsg::InterAgentMessage(event) => {
                inter_agent_index = Some(event_index);
                assert!(event.communication.content.contains("[agent_completed]"));
            }
            EventMsg::TurnCompleted(TurnCompletedEvent {
                turn_id,
                last_assistant_message,
                ..
            }) => {
                turn_completed += 1;
                turn_completed_index = Some(event_index);
                assert_eq!(turn_id, initial_turn_id);
                assert_eq!(last_assistant_message.as_deref(), Some("parent finished"));
                break;
            }
            _ => {}
        }
        event_index += 1;
    }

    assert_eq!(turn_started, 1, "expected exactly one parent turn start");
    assert_eq!(
        turn_completed, 1,
        "expected exactly one parent turn completion"
    );
    assert!(
        matches!(
            (inter_agent_index, turn_completed_index),
            (Some(inter_agent_index), Some(turn_completed_index)) if inter_agent_index < turn_completed_index
        ),
        "expected inline child completion notice before parent turn completion"
    );
    assert!(
        matches!(
            (collab_completion_index, tool_completed_index),
            (Some(collab_completion_index), Some(tool_completed_index)) if collab_completion_index < tool_completed_index
        ),
        "expected spawn_agent tool result after child terminal status"
    );

    std::env::set_current_dir(original_cwd).expect("restore cwd");
}

fn first_user_text(message: &Message) -> Option<String> {
    match message {
        Message::User { content } => content.iter().find_map(|item| match item {
            UserContent::Text(text) => Some(text.text.clone()),
            _ => None,
        }),
        _ => None,
    }
}
