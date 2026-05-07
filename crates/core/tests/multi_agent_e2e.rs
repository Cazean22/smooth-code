use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{Arc, LazyLock, Mutex},
    time::Duration,
};

use anyhow::Result;
use futures_util::{StreamExt, stream};
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
use tokio::sync::{Semaphore, watch};
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

struct ConcurrentSpawnDriver {
    calls: Mutex<usize>,
}

impl SessionModelDriver for ConcurrentSpawnDriver {
    fn stream_turn(&self, _prompt: Message, _history: Vec<Message>) -> Result<SessionStream> {
        unreachable!("manual completion stream should be used for concurrent spawn test");
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
                assert_eq!(
                    first_user_text(&prompt),
                    Some("delegate children".to_string())
                );
                assert!(history.is_empty());
                let tool_call_one = ToolCall::new(
                    "spawn-1".to_string(),
                    ToolFunction::new(
                        "spawn_agent".to_string(),
                        serde_json::json!({
                            "message": "child one",
                            "agent_type": "worker",
                            "fork_context": false
                        }),
                    ),
                )
                .with_call_id("call-1".to_string());
                let tool_call_two = ToolCall::new(
                    "spawn-2".to_string(),
                    ToolFunction::new(
                        "spawn_agent".to_string(),
                        serde_json::json!({
                            "message": "child two",
                            "agent_type": "worker",
                            "fork_context": false
                        }),
                    ),
                )
                .with_call_id("call-2".to_string());
                Ok(Box::pin(stream::iter(vec![
                    Ok(SessionCompletionEvent::AssistantItem(
                        SessionAssistantContent::ToolCall {
                            tool_call: tool_call_one,
                            internal_call_id: "internal-call-1".to_string(),
                        },
                    )),
                    Ok(SessionCompletionEvent::AssistantItem(
                        SessionAssistantContent::ToolCall {
                            tool_call: tool_call_two,
                            internal_call_id: "internal-call-2".to_string(),
                        },
                    )),
                    Ok(SessionCompletionEvent::Completed(SessionTurnSummary {
                        assistant_message_id: Some("assistant-tool-call".to_string()),
                        response: String::new(),
                    })),
                ])))
            }
            1 => {
                assert_eq!(history.len(), 5);
                assert_eq!(
                    first_user_text(&history[0]),
                    Some("delegate children".to_string())
                );
                let first_spawn = tool_result_agent_info(&history[2]);
                let second_spawn = tool_result_agent_info(&history[3]);
                assert_completed_spawn_result(&first_spawn);
                assert_completed_spawn_result(&second_spawn);
                assert_ne!(first_spawn.thread_id, second_spawn.thread_id);
                assert_ne!(first_spawn.agent_path, second_spawn.agent_path);
                assert!(
                    first_user_text(&history[4])
                        .expect("first completion notice text")
                        .contains("[agent_completed]")
                );
                assert!(
                    first_user_text(&prompt)
                        .expect("second completion notice text")
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

struct DeferredChildDriver {
    text: String,
    release: Arc<Semaphore>,
}

impl SessionModelDriver for DeferredChildDriver {
    fn stream_turn(&self, prompt: Message, history: Vec<Message>) -> Result<SessionStream> {
        let _ = (prompt, history);
        let text = self.text.clone();
        let release = Arc::clone(&self.release);
        Ok(Box::pin(
            stream::once(async move {
                let _permit = release.acquire_owned().await.expect("release permit");
                Ok(SessionStreamEvent::StreamAssistantItem(
                    SessionAssistantContent::Text(Text { text }),
                ))
            })
            .chain(stream::iter(vec![Ok(SessionStreamEvent::FinalResponse(
                FinalResponse::empty(),
            ))])),
        ))
    }
}

struct MixedBatchDriver {
    calls: Mutex<usize>,
}

impl SessionModelDriver for MixedBatchDriver {
    fn stream_turn(&self, _prompt: Message, _history: Vec<Message>) -> Result<SessionStream> {
        unreachable!("manual completion stream should be used for mixed batch test");
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
                assert_eq!(first_user_text(&prompt), Some("mixed batch".to_string()));
                assert!(history.is_empty());
                let spawn_tool_call = ToolCall::new(
                    "spawn-1".to_string(),
                    ToolFunction::new(
                        "spawn_agent".to_string(),
                        serde_json::json!({
                            "message": "child one",
                            "agent_type": "worker",
                            "fork_context": false
                        }),
                    ),
                )
                .with_call_id("call-1".to_string());
                let normal_tool_call = ToolCall::new(
                    "tool-2".to_string(),
                    ToolFunction::new(
                        "normal_tool".to_string(),
                        serde_json::json!({
                            "value": "ok"
                        }),
                    ),
                )
                .with_call_id("call-2".to_string());
                Ok(Box::pin(stream::iter(vec![
                    Ok(SessionCompletionEvent::AssistantItem(
                        SessionAssistantContent::ToolCall {
                            tool_call: spawn_tool_call,
                            internal_call_id: "internal-call-1".to_string(),
                        },
                    )),
                    Ok(SessionCompletionEvent::AssistantItem(
                        SessionAssistantContent::ToolCall {
                            tool_call: normal_tool_call,
                            internal_call_id: "internal-call-2".to_string(),
                        },
                    )),
                    Ok(SessionCompletionEvent::Completed(SessionTurnSummary {
                        assistant_message_id: Some("assistant-tool-call".to_string()),
                        response: String::new(),
                    })),
                ])))
            }
            1 => {
                assert_eq!(history.len(), 4);
                assert_eq!(first_user_text(&history[0]), Some("mixed batch".to_string()));
                let spawn_result = tool_result_agent_info(&history[2]);
                assert_completed_spawn_result(&spawn_result);
                assert_eq!(tool_result_text(&history[3]), Some("tool-output".to_string()));
                assert!(
                    first_user_text(&prompt)
                        .expect("completion notice text")
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

    fn call_tool(&self, tool_name: &str, args: &str) -> Result<String> {
        assert_eq!(tool_name, "normal_tool");
        assert_eq!(args, r#"{"value":"ok"}"#);
        Ok("tool-output".to_string())
    }
}

struct MixedBatchFactory {
    build_count: Mutex<usize>,
    child_release: Arc<Semaphore>,
}

impl SessionModelFactory for MixedBatchFactory {
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
            SessionModel::Stub(Arc::new(MixedBatchDriver {
                calls: Mutex::new(0),
            }))
        } else {
            SessionModel::Stub(Arc::new(DeferredChildDriver {
                text: format!("child:{thread_id}"),
                release: Arc::clone(&self.child_release),
            }))
        };
        *build_count += 1;
        Ok(model)
    }
}

struct ConcurrentSpawnFactory {
    build_count: Mutex<usize>,
    child_release: Arc<Semaphore>,
}

impl SessionModelFactory for ConcurrentSpawnFactory {
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
            SessionModel::Stub(Arc::new(ConcurrentSpawnDriver {
                calls: Mutex::new(0),
            }))
        } else {
            SessionModel::Stub(Arc::new(DeferredChildDriver {
                text: format!("child:{thread_id}"),
                release: Arc::clone(&self.child_release),
            }))
        };
        *build_count += 1;
        Ok(model)
    }
}

#[tokio::test]
async fn spawn_agent_waits_for_two_children_and_finishes_in_same_parent_turn() {
    let _cwd_guard = CWD_LOCK.lock().expect("cwd lock");
    let workspace = TempDir::new().expect("tempdir");
    let original_cwd = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(workspace.path()).expect("set cwd");
    let child_release = Arc::new(Semaphore::new(0));
    let manager = ThreadManagerState::new(
        None,
        Some(Arc::new(ConcurrentSpawnFactory {
            build_count: Mutex::new(0),
            child_release: Arc::clone(&child_release),
        })),
    )
    .await
    .expect("thread manager");
    let started = manager.start_thread().await.expect("start root");
    let root_id = started.thread_id;
    let mut root_events = manager.subscribe(root_id).await.expect("subscribe root");
    let client = manager.multi_agent_client(root_id);
    let initial_turn_id = manager
        .start_user_input(root_id, "delegate children".to_string())
        .await
        .expect("start root turn");

    let mut turn_started = 0;
    let mut spawn_tool_calls_started = 0;
    let mut spawn_tool_calls_completed_before_release = 0;
    let mut turn_completed_before_release = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline && spawn_tool_calls_started < 2 {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let event = match tokio::time::timeout(remaining, root_events.recv()).await {
            Ok(Ok(event)) => event,
            Ok(Err(err)) => panic!("root event channel closed: {err}"),
            Err(_) => break,
        };

        match event.msg {
            EventMsg::TurnStarted(TurnStartedEvent { turn_id, .. }) => {
                if turn_id == initial_turn_id {
                    turn_started += 1;
                }
            }
            EventMsg::ToolCallStarted(event) => {
                if matches!(
                    event.call_id.as_str(),
                    "internal-call-1" | "internal-call-2"
                ) {
                    spawn_tool_calls_started += 1;
                }
            }
            EventMsg::ToolCallCompleted(event) => {
                if matches!(
                    event.call_id.as_str(),
                    "internal-call-1" | "internal-call-2"
                ) {
                    spawn_tool_calls_completed_before_release += 1;
                }
            }
            EventMsg::TurnCompleted(TurnCompletedEvent { turn_id, .. }) => {
                if turn_id == initial_turn_id {
                    turn_completed_before_release = true;
                }
            }
            _ => {}
        }
    }

    assert_eq!(turn_started, 1, "expected one parent turn start");
    assert_eq!(
        spawn_tool_calls_started, 2,
        "expected both spawn_agent calls to start before any child finished"
    );
    assert!(
        spawn_tool_calls_completed_before_release == 0,
        "expected spawn_agent tool results to remain pending while children are running"
    );
    assert!(
        !turn_completed_before_release,
        "expected parent turn to remain open while children are running"
    );

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let spawned_agents = loop {
        let listed = client
            .list_agents(Some("/root".to_string()))
            .await
            .expect("list agents while children run")
            .into_iter()
            .filter(|agent| agent.thread_id != root_id.to_string())
            .collect::<Vec<_>>();
        if listed.len() == 2 || tokio::time::Instant::now() >= deadline {
            break listed;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert_eq!(spawned_agents.len(), 2, "expected two live child agents");
    for agent in &spawned_agents {
        assert_live_spawn_result(agent);
    }

    let expected_child_ids = spawned_agents
        .iter()
        .map(|agent| agent.thread_id.clone())
        .collect::<HashSet<_>>();
    let expected_paths = spawned_agents
        .iter()
        .map(|agent| agent.agent_path.clone())
        .collect::<HashSet<_>>();

    child_release.add_permits(2);

    let mut completed_children = HashSet::new();
    let mut completed_tool_results = HashSet::new();
    let mut completion_notices = HashSet::new();
    let mut initial_turn_completed = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let event = match tokio::time::timeout(remaining, root_events.recv()).await {
            Ok(Ok(event)) => event,
            Ok(Err(err)) => panic!("root event channel closed: {err}"),
            Err(_) => break,
        };

        match event.msg {
            EventMsg::TurnStarted(TurnStartedEvent { turn_id, .. }) => {
                assert_eq!(
                    turn_id, initial_turn_id,
                    "did not expect a follow-up parent turn"
                );
                turn_started += 1;
            }
            EventMsg::CollabAgentCompleted(event) => {
                assert_eq!(event.parent_thread_id, root_id);
                assert!(expected_child_ids.contains(&event.child_thread_id.to_string()));
                completed_children.insert(event.child_thread_id.to_string());
            }
            EventMsg::ToolCallCompleted(event) => {
                if matches!(
                    event.call_id.as_str(),
                    "internal-call-1" | "internal-call-2"
                ) {
                    let parsed: AgentInfo = serde_json::from_str(
                        event
                            .output_preview
                            .as_deref()
                            .expect("spawn_agent output preview"),
                    )
                    .expect("spawn_agent result json");
                    assert_completed_spawn_result(&parsed);
                    completed_tool_results.insert(parsed.thread_id);
                }
            }
            EventMsg::InterAgentMessage(event) => {
                if expected_paths.contains(event.communication.author.as_str()) {
                    assert!(event.communication.content.contains("[agent_completed]"));
                    completion_notices.insert(event.communication.author.to_string());
                }
            }
            EventMsg::TurnCompleted(TurnCompletedEvent {
                turn_id,
                last_assistant_message,
                ..
            }) => {
                assert_eq!(turn_id, initial_turn_id);
                assert_eq!(last_assistant_message.as_deref(), Some("parent finished"));
                initial_turn_completed = true;
            }
            _ => {}
        }

        if completed_children.len() == 2
            && completed_tool_results.len() == 2
            && completion_notices.len() == 2
            && initial_turn_completed
        {
            break;
        }
    }

    assert_eq!(
        turn_started, 1,
        "expected the parent to stay on the same turn"
    );
    assert_eq!(
        completed_children.len(),
        2,
        "expected both children to complete"
    );
    assert_eq!(
        completed_tool_results.len(),
        2,
        "expected both spawn_agent tool results to return final child statuses"
    );
    assert_eq!(
        completion_notices.len(),
        2,
        "expected both completion notices to be surfaced inside the same turn"
    );
    assert!(
        initial_turn_completed,
        "expected initial turn to finish after both children"
    );

    std::env::set_current_dir(original_cwd).expect("restore cwd");
}

#[tokio::test]
async fn mixed_spawn_and_normal_tool_results_preserve_model_order() {
    let _cwd_guard = CWD_LOCK.lock().expect("cwd lock");
    let workspace = TempDir::new().expect("tempdir");
    let original_cwd = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(workspace.path()).expect("set cwd");
    let child_release = Arc::new(Semaphore::new(0));
    let manager = ThreadManagerState::new(
        None,
        Some(Arc::new(MixedBatchFactory {
            build_count: Mutex::new(0),
            child_release: Arc::clone(&child_release),
        })),
    )
    .await
    .expect("thread manager");
    let started = manager.start_thread().await.expect("start root");
    let root_id = started.thread_id;
    let mut root_events = manager.subscribe(root_id).await.expect("subscribe root");
    manager
        .start_user_input(root_id, "mixed batch".to_string())
        .await
        .expect("start root turn");

    let mut tool_calls_started = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline && tool_calls_started < 2 {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let event = match tokio::time::timeout(remaining, root_events.recv()).await {
            Ok(Ok(event)) => event,
            Ok(Err(err)) => panic!("root event channel closed: {err}"),
            Err(_) => break,
        };

        if matches!(event.msg, EventMsg::ToolCallStarted(_)) {
            tool_calls_started += 1;
        }
    }

    assert_eq!(tool_calls_started, 2, "expected both tool calls to start");

    child_release.add_permits(1);

    let mut turn_completed = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let event = match tokio::time::timeout(remaining, root_events.recv()).await {
            Ok(Ok(event)) => event,
            Ok(Err(err)) => panic!("root event channel closed: {err}"),
            Err(_) => break,
        };

        if let EventMsg::TurnCompleted(TurnCompletedEvent {
            last_assistant_message,
            ..
        }) = event.msg
        {
            assert_eq!(last_assistant_message.as_deref(), Some("parent finished"));
            turn_completed = true;
            break;
        }
    }

    assert!(turn_completed, "expected mixed batch turn to complete");

    std::env::set_current_dir(original_cwd).expect("restore cwd");
}

fn assert_live_spawn_result(agent: &AgentInfo) {
    assert!(
        matches!(
            agent.status_detail,
            Some(AgentStatus::PendingInit) | Some(AgentStatus::Running)
        ),
        "expected a live child status, got {:?}",
        agent.status_detail
    );
    assert!(agent.last_assistant_message.is_none());
    assert!(!agent.thread_id.is_empty());
    assert!(agent.agent_path.starts_with("/root/"));
}

fn assert_completed_spawn_result(agent: &AgentInfo) {
    assert!(
        matches!(agent.status_detail, Some(AgentStatus::Completed(_))),
        "expected a completed child status, got {:?}",
        agent.status_detail
    );
    assert_eq!(agent.status.as_deref(), Some("completed"));
    assert!(agent.last_assistant_message.is_some());
    assert!(!agent.thread_id.is_empty());
    assert!(agent.agent_path.starts_with("/root/"));
}

fn tool_result_agent_info(message: &Message) -> AgentInfo {
    let tool_result = match message {
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
    serde_json::from_str(&tool_result_text)
        .unwrap_or_else(|err| panic!("spawn_agent output json: {err}; payload={tool_result_text}"))
}

fn tool_result_text(message: &Message) -> Option<String> {
    let tool_result = match message {
        Message::User { content } => content.iter().find_map(|item| match item {
            UserContent::ToolResult(tool_result) => Some(tool_result),
            _ => None,
        }),
        _ => None,
    }?;

    tool_result.content.iter().find_map(|item| match item {
        rig::message::ToolResultContent::Text(text) => Some(text.text.clone()),
        _ => None,
    })
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
