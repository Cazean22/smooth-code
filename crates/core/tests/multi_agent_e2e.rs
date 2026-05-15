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
use serde::Deserialize;
use smooth_core::{
    AgentControl, RoleOverride, SessionAssistantContent, SessionCompletionEvent,
    SessionCompletionStream, SessionModel, SessionModelDriver, SessionModelFactory, SessionStream,
    SessionStreamEvent, SessionTurnSummary, ThreadManagerState,
};
use smooth_protocol::{
    AgentStatus, CollabAgentStatusEntry, EventMsg, ThreadId, TurnCompletedEvent, TurnStartedEvent,
};
use tempfile::TempDir;
use tokio::sync::{RwLock, Semaphore};
use tools::DynamicToolClient;

static CWD_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct TestAgentInfo {
    thread_id: String,
    agent_path: String,
    agent_nickname: Option<String>,
    agent_role: Option<String>,
    status: Option<String>,
    #[serde(default)]
    status_detail: Option<AgentStatus>,
    #[serde(default)]
    last_assistant_message: Option<String>,
}

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
        _current_turn_id: Arc<RwLock<Option<String>>>,
        _role_override: RoleOverride,
        _agent_control: AgentControl,
    ) -> Result<SessionModel> {
        Ok(SessionModel::Stub(Arc::new(StubDriver {
            text: format!("done:{thread_id}"),
        })))
    }
}

#[tokio::test]
async fn agent_control_round_trip_spawns_lists_closes_and_emits_completion() {
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

    let spawned = manager
        .spawn_agent_with_role(
            root_id,
            "inspect workspace".to_string(),
            Some("explorer".to_string()),
            None,
            false,
        )
        .await
        .expect("spawn should succeed");
    assert!(spawned.agent_path.as_str().starts_with("/root/"));
    assert_eq!(spawned.agent_role.as_deref(), Some("explorer"));

    let mut saw_completion = false;
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
                assert_eq!(event.child_thread_id, spawned.thread_id);
                assert_eq!(event.agent_path, spawned.agent_path);
                assert_eq!(
                    event.status,
                    AgentStatus::Completed(Some(format!("done:{}", spawned.thread_id)))
                );
                saw_completion = true;
            }
            _ => {}
        }

        if saw_completion {
            break;
        }
    }
    assert!(
        saw_completion,
        "expected child completion event on parent thread"
    );

    let listed = manager
        .list_agents(root_id, Some("/root"))
        .expect("list should succeed");
    assert_eq!(listed.len(), 2);
    assert!(
        listed
            .iter()
            .any(|agent| agent.agent_path == spawned.agent_path)
    );

    let closed = manager
        .close_agent(root_id, spawned.agent_path.as_str())
        .await
        .expect("close should succeed");
    assert_eq!(closed, AgentStatus::Shutdown);

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
                assert_eq!(history.len(), 3);
                assert_eq!(
                    first_user_text(&history[0]),
                    Some("delegate children".to_string())
                );
                let first_spawn = tool_result_agent_info(&history[2]);
                let second_spawn = tool_result_agent_info(&prompt);
                assert_completed_spawn_result(&first_spawn);
                assert_completed_spawn_result(&second_spawn);
                assert_ne!(first_spawn.thread_id, second_spawn.thread_id);
                assert_ne!(first_spawn.agent_path, second_spawn.agent_path);

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
                release.acquire().await.expect("release permit").forget();
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
                assert_eq!(history.len(), 3);
                assert_eq!(
                    first_user_text(&history[0]),
                    Some("mixed batch".to_string())
                );
                let spawn_result = tool_result_agent_info(&history[2]);
                assert_live_spawn_result(&spawn_result);
                assert_eq!(tool_result_text(&prompt), Some("tool-output".to_string()));

                Ok(Box::pin(stream::iter(vec![Ok(
                    SessionCompletionEvent::Completed(SessionTurnSummary {
                        assistant_message_id: Some("assistant-waiting".to_string()),
                        response: String::new(),
                    }),
                )])))
            }
            2 => {
                assert_eq!(history.len(), 4);
                assert_eq!(
                    first_user_text(&history[0]),
                    Some("mixed batch".to_string())
                );
                let spawn_result = tool_result_agent_info(&history[2]);
                assert_live_spawn_result(&spawn_result);
                assert_eq!(
                    tool_result_text(&history[3]),
                    Some("tool-output".to_string())
                );
                let completed = user_text_agent_info(&prompt);
                assert_completed_spawn_result(&completed);

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

struct TwoRetainedDriver {
    calls: Mutex<usize>,
}

impl SessionModelDriver for TwoRetainedDriver {
    fn stream_turn(&self, _prompt: Message, _history: Vec<Message>) -> Result<SessionStream> {
        unreachable!("manual completion stream should be used for two retained test");
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
                assert_eq!(first_user_text(&prompt), Some("two retained".to_string()));
                assert!(history.is_empty());
                let spawn_tool_call_one = ToolCall::new(
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
                let spawn_tool_call_two = ToolCall::new(
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
                let normal_tool_call = ToolCall::new(
                    "tool-3".to_string(),
                    ToolFunction::new(
                        "normal_tool".to_string(),
                        serde_json::json!({
                            "value": "ok"
                        }),
                    ),
                )
                .with_call_id("call-3".to_string());
                Ok(Box::pin(stream::iter(vec![
                    Ok(SessionCompletionEvent::AssistantItem(
                        SessionAssistantContent::ToolCall {
                            tool_call: spawn_tool_call_one,
                            internal_call_id: "internal-call-1".to_string(),
                        },
                    )),
                    Ok(SessionCompletionEvent::AssistantItem(
                        SessionAssistantContent::ToolCall {
                            tool_call: spawn_tool_call_two,
                            internal_call_id: "internal-call-2".to_string(),
                        },
                    )),
                    Ok(SessionCompletionEvent::AssistantItem(
                        SessionAssistantContent::ToolCall {
                            tool_call: normal_tool_call,
                            internal_call_id: "internal-call-3".to_string(),
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
                assert_eq!(
                    first_user_text(&history[0]),
                    Some("two retained".to_string())
                );
                assert_live_spawn_result(&tool_result_agent_info(&history[2]));
                assert_live_spawn_result(&tool_result_agent_info(&history[3]));
                assert_eq!(tool_result_text(&prompt), Some("tool-output".to_string()));

                Ok(Box::pin(stream::iter(vec![Ok(
                    SessionCompletionEvent::Completed(SessionTurnSummary {
                        assistant_message_id: Some("assistant-waiting".to_string()),
                        response: String::new(),
                    }),
                )])))
            }
            2 => {
                assert_eq!(history.len(), 6);
                assert_eq!(
                    tool_result_text(&history[4]),
                    Some("tool-output".to_string())
                );
                assert_completed_spawn_result(&user_text_agent_info(&history[5]));
                assert_completed_spawn_result(&user_text_agent_info(&prompt));

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

struct TwoRetainedFactory {
    build_count: Mutex<usize>,
    child_release: Arc<Semaphore>,
}

impl SessionModelFactory for TwoRetainedFactory {
    fn build(
        &self,
        _cwd: PathBuf,
        thread_id: ThreadId,
        _dynamic_tool_client: Option<Arc<dyn DynamicToolClient>>,
        _current_turn_id: Arc<RwLock<Option<String>>>,
        _role_override: RoleOverride,
        _agent_control: AgentControl,
    ) -> Result<SessionModel> {
        let mut build_count = self.build_count.lock().expect("build count mutex");
        let model = if *build_count == 0 {
            SessionModel::Stub(Arc::new(TwoRetainedDriver {
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

impl SessionModelFactory for MixedBatchFactory {
    fn build(
        &self,
        _cwd: PathBuf,
        thread_id: ThreadId,
        _dynamic_tool_client: Option<Arc<dyn DynamicToolClient>>,
        _current_turn_id: Arc<RwLock<Option<String>>>,
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
        _current_turn_id: Arc<RwLock<Option<String>>>,
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
        let listed = manager
            .list_agents(root_id, Some("/root"))
            .expect("list agents while children run")
            .into_iter()
            .filter(|agent| agent.thread_id != root_id)
            .collect::<Vec<_>>();
        if listed.len() == 2 || tokio::time::Instant::now() >= deadline {
            break listed;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert_eq!(spawned_agents.len(), 2, "expected two live child agents");
    for agent in &spawned_agents {
        assert_live_status_entry(agent);
    }

    let expected_child_ids = spawned_agents
        .iter()
        .map(|agent| agent.thread_id.to_string())
        .collect::<HashSet<_>>();
    child_release.add_permits(2);

    let mut completed_children = HashSet::new();
    let mut completed_tool_results = HashSet::new();
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
                    let parsed: TestAgentInfo = serde_json::from_str(
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
    let initial_turn_id = manager
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

    let mut saw_live_spawn_result = false;
    let mut saw_normal_result = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline && !(saw_live_spawn_result && saw_normal_result) {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let event = match tokio::time::timeout(remaining, root_events.recv()).await {
            Ok(Ok(event)) => event,
            Ok(Err(err)) => panic!("root event channel closed: {err}"),
            Err(_) => break,
        };

        if let EventMsg::ToolCallCompleted(event) = event.msg {
            match event.call_id.as_str() {
                "internal-call-1" => {
                    let parsed: TestAgentInfo = serde_json::from_str(
                        event
                            .output_preview
                            .as_deref()
                            .expect("spawn_agent output preview"),
                    )
                    .expect("spawn_agent result json");
                    assert_live_spawn_result(&parsed);
                    saw_live_spawn_result = true;
                }
                "internal-call-2" => {
                    assert_eq!(event.output_preview.as_deref(), Some("tool-output"));
                    saw_normal_result = true;
                }
                _ => {}
            }
        }
    }

    assert!(
        saw_live_spawn_result,
        "expected mixed spawn_agent result to return live status after grace period"
    );
    assert!(saw_normal_result, "expected normal tool result");

    let deadline = tokio::time::Instant::now() + Duration::from_millis(250);
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let event = match tokio::time::timeout(remaining, root_events.recv()).await {
            Ok(Ok(event)) => event,
            Ok(Err(err)) => panic!("root event channel closed: {err}"),
            Err(_) => break,
        };

        match event.msg {
            EventMsg::TurnCompleted(TurnCompletedEvent { turn_id, .. }) => {
                panic!("turn {turn_id} completed before retained subagent finished");
            }
            EventMsg::TurnStarted(TurnStartedEvent { turn_id, .. }) => {
                assert_eq!(turn_id, initial_turn_id, "did not expect a follow-up turn");
            }
            _ => {}
        }
    }

    child_release.add_permits(1);

    let mut saw_child_completion = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let event = match tokio::time::timeout(remaining, root_events.recv()).await {
            Ok(Ok(event)) => event,
            Ok(Err(err)) => panic!("root event channel closed: {err}"),
            Err(_) => break,
        };

        if matches!(event.msg, EventMsg::CollabAgentCompleted(_)) {
            saw_child_completion = true;
            break;
        }
    }
    assert!(
        saw_child_completion,
        "expected child to complete before the retained parent turn finishes"
    );

    let mut turn_completed = false;
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
                assert_eq!(turn_id, initial_turn_id, "did not expect a follow-up turn");
            }
            EventMsg::TurnCompleted(TurnCompletedEvent {
                turn_id,
                last_assistant_message,
                ..
            }) => {
                assert_eq!(turn_id, initial_turn_id);
                assert_eq!(last_assistant_message.as_deref(), Some("parent finished"));
                turn_completed = true;
                break;
            }
            _ => {}
        }
    }

    assert!(turn_completed, "expected mixed batch turn to complete");

    std::env::set_current_dir(original_cwd).expect("restore cwd");
}

#[tokio::test]
async fn retained_subagents_all_finish_before_parent_continues() {
    let _cwd_guard = CWD_LOCK.lock().expect("cwd lock");
    let workspace = TempDir::new().expect("tempdir");
    let original_cwd = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(workspace.path()).expect("set cwd");
    let child_release = Arc::new(Semaphore::new(0));
    let manager = ThreadManagerState::new(
        None,
        Some(Arc::new(TwoRetainedFactory {
            build_count: Mutex::new(0),
            child_release: Arc::clone(&child_release),
        })),
    )
    .await
    .expect("thread manager");
    let started = manager.start_thread().await.expect("start root");
    let root_id = started.thread_id;
    let mut root_events = manager.subscribe(root_id).await.expect("subscribe root");
    let initial_turn_id = manager
        .start_user_input(root_id, "two retained".to_string())
        .await
        .expect("start root turn");

    let mut tool_calls_completed = HashSet::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline && tool_calls_completed.len() < 3 {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let event = match tokio::time::timeout(remaining, root_events.recv()).await {
            Ok(Ok(event)) => event,
            Ok(Err(err)) => panic!("root event channel closed: {err}"),
            Err(_) => break,
        };

        if let EventMsg::ToolCallCompleted(event) = event.msg {
            tool_calls_completed.insert(event.call_id);
        }
    }

    assert_eq!(tool_calls_completed.len(), 3, "expected all tool results");

    child_release.add_permits(1);
    let mut saw_one_child_completion = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline && !saw_one_child_completion {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let event = match tokio::time::timeout(remaining, root_events.recv()).await {
            Ok(Ok(event)) => event,
            Ok(Err(err)) => panic!("root event channel closed: {err}"),
            Err(_) => break,
        };

        match event.msg {
            EventMsg::CollabAgentCompleted(_) => saw_one_child_completion = true,
            EventMsg::TurnCompleted(TurnCompletedEvent { turn_id, .. }) => {
                panic!("turn {turn_id} completed before all retained subagents finished");
            }
            EventMsg::TurnStarted(TurnStartedEvent { turn_id, .. }) => {
                assert_eq!(turn_id, initial_turn_id, "did not expect a follow-up turn");
            }
            _ => {}
        }
    }
    assert!(saw_one_child_completion, "expected one child completion");

    let deadline = tokio::time::Instant::now() + Duration::from_millis(250);
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let event = match tokio::time::timeout(remaining, root_events.recv()).await {
            Ok(Ok(event)) => event,
            Ok(Err(err)) => panic!("root event channel closed: {err}"),
            Err(_) => break,
        };

        if let EventMsg::TurnCompleted(TurnCompletedEvent { turn_id, .. }) = event.msg {
            panic!("turn {turn_id} completed before second retained subagent finished");
        }
    }

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

        match event.msg {
            EventMsg::TurnStarted(TurnStartedEvent { turn_id, .. }) => {
                assert_eq!(turn_id, initial_turn_id, "did not expect a follow-up turn");
            }
            EventMsg::TurnCompleted(TurnCompletedEvent {
                turn_id,
                last_assistant_message,
                ..
            }) => {
                assert_eq!(turn_id, initial_turn_id);
                assert_eq!(last_assistant_message.as_deref(), Some("parent finished"));
                turn_completed = true;
                break;
            }
            _ => {}
        }
    }

    assert!(
        turn_completed,
        "expected parent to finish after both children"
    );

    std::env::set_current_dir(original_cwd).expect("restore cwd");
}

fn assert_live_status_entry(agent: &CollabAgentStatusEntry) {
    assert!(
        matches!(
            agent.status,
            AgentStatus::PendingInit | AgentStatus::Running
        ),
        "expected a live child status, got {:?}",
        agent.status
    );
    assert!(agent.last_assistant_message.is_none());
    assert!(agent.agent_path.as_str().starts_with("/root/"));
}

fn assert_live_spawn_result(agent: &TestAgentInfo) {
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

fn assert_completed_spawn_result(agent: &TestAgentInfo) {
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

fn tool_result_agent_info(message: &Message) -> TestAgentInfo {
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

fn user_text_agent_info(message: &Message) -> TestAgentInfo {
    let text = first_user_text(message).expect("user text spawn result");
    serde_json::from_str(&text)
        .unwrap_or_else(|err| panic!("spawn_agent user text json: {err}; payload={text}"))
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
