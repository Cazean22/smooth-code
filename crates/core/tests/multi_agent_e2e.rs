use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{Arc, LazyLock, Mutex},
    time::Duration,
};

use anyhow::Result;
use futures_util::{StreamExt, stream};
use rig::message::{
    AssistantContent, Message, Reasoning, ReasoningContent, Text, ToolCall, ToolFunction,
    UserContent,
};
use serde::Deserialize;
use smooth_core::{
    AgentControl, SessionAssistantContent, SessionCompletionEvent, SessionCompletionStream,
    SessionModel, SessionModelDriver, SessionModelFactory, SessionTurnSummary, SystemPromptKind,
    ThreadManagerState,
};
use smooth_protocol::{
    AgentStatus, CollabAgentStatusEntry, EventMsg, ThreadId, ToolCallResultKind,
    TurnCompletedEvent, TurnStartedEvent,
};
use tempfile::TempDir;
use tokio::sync::{RwLock, Semaphore};
use tools::AskUserClient;

static CWD_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct TestAgentInfo {
    event: Option<String>,
    thread_id: String,
    agent_path: String,
    agent_nickname: Option<String>,
    status: Option<String>,
    #[serde(default)]
    status_detail: Option<AgentStatus>,
    #[serde(default)]
    last_assistant_message: Option<String>,
    #[serde(default)]
    next_action: Option<String>,
    #[serde(default)]
    instructions: Option<String>,
}

struct StubDriver {
    text: String,
}

impl SessionModelDriver for StubDriver {
    fn stream_completion_turn(
        &self,
        prompt: Message,
        history: Vec<Message>,
    ) -> Result<SessionCompletionStream> {
        let _ = (prompt, history);
        let text = self.text.clone();
        Ok(Box::pin(stream::iter(vec![
            Ok(SessionCompletionEvent::AssistantItem(
                SessionAssistantContent::Text(Text {
                    text: self.text.clone(),
                }),
            )),
            Ok(SessionCompletionEvent::Completed(SessionTurnSummary {
                assistant_message_id: Some("assistant-stub".to_string()),
                response: text,
            })),
        ])))
    }
}

struct AnyThreadFactory;

impl SessionModelFactory for AnyThreadFactory {
    fn build(
        &self,
        _cwd: PathBuf,
        thread_id: ThreadId,
        _ask_user_client: Option<AskUserClient>,
        _current_turn_id: Arc<RwLock<Option<String>>>,
        _system_prompt_kind: SystemPromptKind,
        _agent_control: AgentControl,
        _plan_mode: bool,
    ) -> Result<SessionModel> {
        Ok(SessionModel::Stub(Arc::new(StubDriver {
            text: format!("done:{thread_id}"),
        })))
    }
}

struct ExploreRoutingParentDriver {
    calls: Mutex<usize>,
}

impl SessionModelDriver for ExploreRoutingParentDriver {
    fn stream_completion_turn(
        &self,
        prompt: Message,
        history: Vec<Message>,
    ) -> Result<SessionCompletionStream> {
        let mut calls = self
            .calls
            .lock()
            .map_err(|_| anyhow::anyhow!("calls mutex"))?;
        let call_idx = *calls;
        *calls += 1;
        drop(calls);

        match call_idx {
            0 => {
                assert_eq!(first_user_text(&prompt), Some("route explore".to_string()));
                assert!(history.is_empty());
                let tool_call = ToolCall::new(
                    "spawn-explore".to_string(),
                    ToolFunction::new(
                        "spawn_agent".to_string(),
                        serde_json::json!({
                            "description": "explore core",
                            "prompt": "inspect architecture",
                            "subagent_type": "Explore"
                        }),
                    ),
                )
                .with_call_id("call-explore".to_string());
                Ok(Box::pin(stream::iter(vec![
                    Ok(SessionCompletionEvent::AssistantItem(
                        SessionAssistantContent::ToolCall {
                            tool_call,
                            internal_call_id: "internal-call-explore".to_string(),
                        },
                    )),
                    Ok(SessionCompletionEvent::Completed(SessionTurnSummary {
                        assistant_message_id: Some("assistant-spawn-explore".to_string()),
                        response: String::new(),
                    })),
                ])))
            }
            1 => {
                assert_eq!(history.len(), 2);
                let results = tool_result_agent_infos(&prompt);
                assert_eq!(results.len(), 1);
                assert_completed_spawn_result(&results[0]);
                assert_eq!(
                    results[0].last_assistant_message.as_deref(),
                    Some("explore findings")
                );

                Ok(Box::pin(stream::iter(vec![
                    Ok(SessionCompletionEvent::AssistantItem(
                        SessionAssistantContent::Text(Text {
                            text: "parent saw explore".to_string(),
                        }),
                    )),
                    Ok(SessionCompletionEvent::Completed(SessionTurnSummary {
                        assistant_message_id: Some("assistant-final".to_string()),
                        response: "parent saw explore".to_string(),
                    })),
                ])))
            }
            other => panic!("unexpected completion turn {other}"),
        }
    }
}

struct ExploreRoutingChildDriver {
    child_inputs: Arc<Mutex<Vec<(String, usize)>>>,
}

impl SessionModelDriver for ExploreRoutingChildDriver {
    fn stream_completion_turn(
        &self,
        prompt: Message,
        history: Vec<Message>,
    ) -> Result<SessionCompletionStream> {
        let prompt_text =
            first_user_text(&prompt).ok_or_else(|| anyhow::anyhow!("missing child prompt"))?;
        let history_len = history.len();
        self.child_inputs
            .lock()
            .map_err(|_| anyhow::anyhow!("child input mutex"))?
            .push((prompt_text, history_len));

        let text = "explore findings".to_string();
        Ok(Box::pin(stream::iter(vec![
            Ok(SessionCompletionEvent::AssistantItem(
                SessionAssistantContent::Text(Text { text: text.clone() }),
            )),
            Ok(SessionCompletionEvent::Completed(SessionTurnSummary {
                assistant_message_id: Some("assistant-explore-child".to_string()),
                response: text,
            })),
        ])))
    }
}

struct ExploreRoutingFactory {
    builds: Arc<Mutex<Vec<(ThreadId, SystemPromptKind)>>>,
    child_inputs: Arc<Mutex<Vec<(String, usize)>>>,
}

impl SessionModelFactory for ExploreRoutingFactory {
    fn build(
        &self,
        _cwd: PathBuf,
        thread_id: ThreadId,
        _ask_user_client: Option<AskUserClient>,
        _current_turn_id: Arc<RwLock<Option<String>>>,
        system_prompt_kind: SystemPromptKind,
        _agent_control: AgentControl,
        _plan_mode: bool,
    ) -> Result<SessionModel> {
        self.builds
            .lock()
            .map_err(|_| anyhow::anyhow!("builds mutex"))?
            .push((thread_id, system_prompt_kind));
        match system_prompt_kind {
            SystemPromptKind::Root => {
                Ok(SessionModel::Stub(Arc::new(ExploreRoutingParentDriver {
                    calls: Mutex::new(0),
                })))
            }
            SystemPromptKind::Explore => {
                Ok(SessionModel::Stub(Arc::new(ExploreRoutingChildDriver {
                    child_inputs: Arc::clone(&self.child_inputs),
                })))
            }
            SystemPromptKind::DefaultSubagent => Err(anyhow::anyhow!(
                "Explore subagent_type should not spawn a default subagent"
            )),
        }
    }
}

#[tokio::test]
async fn agent_control_round_trip_spawns_lists_closes_and_emits_completion() -> Result<()> {
    let _cwd_guard = CWD_LOCK.lock().map_err(|_| anyhow::anyhow!("cwd lock"))?;
    let workspace = TempDir::new()?;
    let original_cwd = std::env::current_dir()?;
    std::env::set_current_dir(workspace.path())?;

    let manager = ThreadManagerState::new(None, Some(Arc::new(AnyThreadFactory))).await?;
    let started = manager.start_thread().await?;
    let root_id = started.thread_id;
    let mut root_events = manager.subscribe(root_id).await?;

    let spawned = manager
        .spawn_agent(root_id, "inspect workspace".to_string())
        .await?;
    assert!(spawned.agent_path.as_str().starts_with("/root/"));

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

    let listed = manager.list_agents(root_id, Some("/root"))?;
    assert_eq!(listed.len(), 2);
    assert!(
        listed
            .iter()
            .any(|agent| agent.agent_path == spawned.agent_path)
    );

    let closed = manager
        .close_agent(root_id, spawned.agent_path.as_str())
        .await?;
    assert_eq!(closed, AgentStatus::Shutdown);

    std::env::set_current_dir(original_cwd)?;
    Ok(())
}

#[tokio::test]
async fn spawn_agent_subagent_type_explore_routes_to_explore_prompt_kind() -> Result<()> {
    let _cwd_guard = CWD_LOCK.lock().map_err(|_| anyhow::anyhow!("cwd lock"))?;
    let workspace = TempDir::new()?;
    let original_cwd = std::env::current_dir()?;
    std::env::set_current_dir(workspace.path())?;
    let builds = Arc::new(Mutex::new(Vec::new()));
    let child_inputs = Arc::new(Mutex::new(Vec::new()));
    let manager = ThreadManagerState::new(
        None,
        Some(Arc::new(ExploreRoutingFactory {
            builds: Arc::clone(&builds),
            child_inputs: Arc::clone(&child_inputs),
        })),
    )
    .await?;
    let started = manager.start_thread().await?;
    let root_id = started.thread_id;
    let mut root_events = manager.subscribe(root_id).await?;

    let turn_id = manager
        .start_user_input(root_id, "route explore".to_string())
        .await?;
    wait_for_turn_completion(&mut root_events, &turn_id).await;

    let builds = builds
        .lock()
        .map_err(|_| anyhow::anyhow!("builds mutex"))?
        .clone();
    assert!(
        builds
            .iter()
            .any(|(thread_id, kind)| *thread_id == root_id && *kind == SystemPromptKind::Root)
    );
    assert!(
        builds
            .iter()
            .any(|(_, kind)| *kind == SystemPromptKind::Explore),
        "expected an Explore child build, got {builds:?}"
    );
    assert!(
        !builds
            .iter()
            .any(|(_, kind)| *kind == SystemPromptKind::DefaultSubagent),
        "Explore subagent_type should not use the default subagent prompt"
    );

    let child_inputs = child_inputs
        .lock()
        .map_err(|_| anyhow::anyhow!("child input mutex"))?
        .clone();
    assert_eq!(
        child_inputs,
        vec![("inspect architecture".to_string(), 0)],
        "Explore child should receive the prompt as input and start with empty history"
    );

    std::env::set_current_dir(original_cwd)?;
    Ok(())
}

struct ConcurrentSpawnDriver {
    calls: Mutex<usize>,
}

impl SessionModelDriver for ConcurrentSpawnDriver {
    fn stream_completion_turn(
        &self,
        prompt: Message,
        history: Vec<Message>,
    ) -> Result<SessionCompletionStream> {
        let mut calls = self
            .calls
            .lock()
            .map_err(|_| anyhow::anyhow!("calls mutex"))?;
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
                            "description": "child one",
                            "prompt": "child one"
                        }),
                    ),
                )
                .with_call_id("call-1".to_string());
                let tool_call_two = ToolCall::new(
                    "spawn-2".to_string(),
                    ToolFunction::new(
                        "spawn_agent".to_string(),
                        serde_json::json!({
                            "description": "child two",
                            "prompt": "child two"
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
                assert_eq!(history.len(), 2);
                assert_eq!(
                    first_user_text(&history[0]),
                    Some("delegate children".to_string())
                );
                let spawns = tool_result_agent_infos(&prompt);
                assert_eq!(spawns.len(), 2);
                let first_spawn = &spawns[0];
                let second_spawn = &spawns[1];
                assert_completed_spawn_result(first_spawn);
                assert_completed_spawn_result(second_spawn);
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
    fn stream_completion_turn(
        &self,
        prompt: Message,
        history: Vec<Message>,
    ) -> Result<SessionCompletionStream> {
        let _ = (prompt, history);
        let text = self.text.clone();
        let completed_text = text.clone();
        let release = Arc::clone(&self.release);
        Ok(Box::pin(
            stream::once(async move {
                release.acquire().await?.forget();
                Ok(SessionCompletionEvent::AssistantItem(
                    SessionAssistantContent::Text(Text { text }),
                ))
            })
            .chain(stream::iter(vec![Ok(SessionCompletionEvent::Completed(
                SessionTurnSummary {
                    assistant_message_id: Some("assistant-deferred-child".to_string()),
                    response: completed_text,
                },
            ))])),
        ))
    }
}

struct MixedBatchDriver {
    calls: Mutex<usize>,
}

impl SessionModelDriver for MixedBatchDriver {
    fn stream_completion_turn(
        &self,
        prompt: Message,
        history: Vec<Message>,
    ) -> Result<SessionCompletionStream> {
        let mut calls = self
            .calls
            .lock()
            .map_err(|_| anyhow::anyhow!("calls mutex"))?;
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
                            "description": "child one",
                            "prompt": "child one"
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
                assert_eq!(history.len(), 2);
                assert_eq!(
                    first_user_text(&history[0]),
                    Some("mixed batch".to_string())
                );
                let tool_results = tool_result_texts(&prompt);
                assert_eq!(tool_results.len(), 2);
                let spawn_result = parse_agent_info(&tool_results[0]);
                assert_live_spawn_result(&spawn_result);
                assert_eq!(tool_results[1], "tool-output");

                Ok(Box::pin(stream::iter(vec![Ok(
                    SessionCompletionEvent::Completed(SessionTurnSummary {
                        assistant_message_id: Some("assistant-waiting".to_string()),
                        response: String::new(),
                    }),
                )])))
            }
            2 => {
                assert_eq!(history.len(), 3);
                assert_eq!(
                    first_user_text(&history[0]),
                    Some("mixed batch".to_string())
                );
                let tool_results = tool_result_texts(&history[2]);
                assert_eq!(tool_results.len(), 2);
                let spawn_result = parse_agent_info(&tool_results[0]);
                assert_live_spawn_result(&spawn_result);
                assert_eq!(tool_results[1], "tool-output");
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
    fn stream_completion_turn(
        &self,
        prompt: Message,
        history: Vec<Message>,
    ) -> Result<SessionCompletionStream> {
        let mut calls = self
            .calls
            .lock()
            .map_err(|_| anyhow::anyhow!("calls mutex"))?;
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
                            "description": "child one",
                            "prompt": "child one"
                        }),
                    ),
                )
                .with_call_id("call-1".to_string());
                let spawn_tool_call_two = ToolCall::new(
                    "spawn-2".to_string(),
                    ToolFunction::new(
                        "spawn_agent".to_string(),
                        serde_json::json!({
                            "description": "child two",
                            "prompt": "child two"
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
                assert_eq!(history.len(), 2);
                assert_eq!(
                    first_user_text(&history[0]),
                    Some("two retained".to_string())
                );
                let tool_results = tool_result_texts(&prompt);
                assert_eq!(tool_results.len(), 3);
                assert_live_spawn_result(&parse_agent_info(&tool_results[0]));
                assert_live_spawn_result(&parse_agent_info(&tool_results[1]));
                assert_eq!(tool_results[2], "tool-output");

                Ok(Box::pin(stream::iter(vec![Ok(
                    SessionCompletionEvent::Completed(SessionTurnSummary {
                        assistant_message_id: Some("assistant-waiting".to_string()),
                        response: String::new(),
                    }),
                )])))
            }
            2 => {
                assert_eq!(history.len(), 3);
                let tool_results = tool_result_texts(&history[2]);
                assert_eq!(tool_results.len(), 3);
                assert_eq!(tool_results[2], "tool-output");
                let completed = user_text_agent_infos(&prompt);
                assert_eq!(completed.len(), 2);
                assert_completed_spawn_result(&completed[0]);
                assert_completed_spawn_result(&completed[1]);

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
        _ask_user_client: Option<AskUserClient>,
        _current_turn_id: Arc<RwLock<Option<String>>>,
        _system_prompt_kind: SystemPromptKind,
        _agent_control: AgentControl,
        _plan_mode: bool,
    ) -> Result<SessionModel> {
        let mut build_count = self
            .build_count
            .lock()
            .map_err(|_| anyhow::anyhow!("build count mutex"))?;
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
        _ask_user_client: Option<AskUserClient>,
        _current_turn_id: Arc<RwLock<Option<String>>>,
        _system_prompt_kind: SystemPromptKind,
        _agent_control: AgentControl,
        _plan_mode: bool,
    ) -> Result<SessionModel> {
        let mut build_count = self
            .build_count
            .lock()
            .map_err(|_| anyhow::anyhow!("build count mutex"))?;
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
        _ask_user_client: Option<AskUserClient>,
        _current_turn_id: Arc<RwLock<Option<String>>>,
        _system_prompt_kind: SystemPromptKind,
        _agent_control: AgentControl,
        _plan_mode: bool,
    ) -> Result<SessionModel> {
        let mut build_count = self
            .build_count
            .lock()
            .map_err(|_| anyhow::anyhow!("build count mutex"))?;
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
async fn spawn_agent_waits_for_two_children_and_finishes_in_same_parent_turn() -> Result<()> {
    let _cwd_guard = CWD_LOCK.lock().map_err(|_| anyhow::anyhow!("cwd lock"))?;
    let workspace = TempDir::new()?;
    let original_cwd = std::env::current_dir()?;
    std::env::set_current_dir(workspace.path())?;
    let child_release = Arc::new(Semaphore::new(0));
    let manager = ThreadManagerState::new(
        None,
        Some(Arc::new(ConcurrentSpawnFactory {
            build_count: Mutex::new(0),
            child_release: Arc::clone(&child_release),
        })),
    )
    .await?;
    let started = manager.start_thread().await?;
    let root_id = started.thread_id;
    let mut root_events = manager.subscribe(root_id).await?;
    let initial_turn_id = manager
        .start_user_input(root_id, "delegate children".to_string())
        .await?;

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
            .list_agents(root_id, Some("/root"))?
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
                            .ok_or_else(|| anyhow::anyhow!("spawn_agent output preview"))?,
                    )?;
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

    std::env::set_current_dir(original_cwd)?;
    Ok(())
}

#[tokio::test]
async fn mixed_spawn_and_normal_tool_results_preserve_model_order() -> Result<()> {
    let _cwd_guard = CWD_LOCK.lock().map_err(|_| anyhow::anyhow!("cwd lock"))?;
    let workspace = TempDir::new()?;
    let original_cwd = std::env::current_dir()?;
    std::env::set_current_dir(workspace.path())?;
    let child_release = Arc::new(Semaphore::new(0));
    let manager = ThreadManagerState::new(
        None,
        Some(Arc::new(MixedBatchFactory {
            build_count: Mutex::new(0),
            child_release: Arc::clone(&child_release),
        })),
    )
    .await?;
    let started = manager.start_thread().await?;
    let root_id = started.thread_id;
    let mut root_events = manager.subscribe(root_id).await?;
    let initial_turn_id = manager
        .start_user_input(root_id, "mixed batch".to_string())
        .await?;

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
                    assert_eq!(event.result_kind, ToolCallResultKind::StatusUpdate);
                    assert!(event.related_thread_id.is_some());
                    let parsed: TestAgentInfo = serde_json::from_str(
                        event
                            .output_preview
                            .as_deref()
                            .ok_or_else(|| anyhow::anyhow!("spawn_agent output preview"))?,
                    )?;
                    assert_live_spawn_result(&parsed);
                    assert_eq!(
                        event
                            .related_thread_id
                            .map(|thread_id| thread_id.to_string()),
                        Some(parsed.thread_id.clone())
                    );
                    saw_live_spawn_result = true;
                }
                "internal-call-2" => {
                    assert_eq!(event.result_kind, ToolCallResultKind::Final);
                    assert_eq!(event.related_thread_id, None);
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

    std::env::set_current_dir(original_cwd)?;
    Ok(())
}

#[tokio::test]
async fn retained_subagents_all_finish_before_parent_continues() -> Result<()> {
    let _cwd_guard = CWD_LOCK.lock().map_err(|_| anyhow::anyhow!("cwd lock"))?;
    let workspace = TempDir::new()?;
    let original_cwd = std::env::current_dir()?;
    std::env::set_current_dir(workspace.path())?;
    let child_release = Arc::new(Semaphore::new(0));
    let manager = ThreadManagerState::new(
        None,
        Some(Arc::new(TwoRetainedFactory {
            build_count: Mutex::new(0),
            child_release: Arc::clone(&child_release),
        })),
    )
    .await?;
    let started = manager.start_thread().await?;
    let root_id = started.thread_id;
    let mut root_events = manager.subscribe(root_id).await?;
    let initial_turn_id = manager
        .start_user_input(root_id, "two retained".to_string())
        .await?;

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

    std::env::set_current_dir(original_cwd)?;
    Ok(())
}

struct ReasoningStreamDriver {
    calls: Mutex<usize>,
}

impl SessionModelDriver for ReasoningStreamDriver {
    fn stream_completion_turn(
        &self,
        prompt: Message,
        history: Vec<Message>,
    ) -> Result<SessionCompletionStream> {
        let mut calls = self
            .calls
            .lock()
            .map_err(|_| anyhow::anyhow!("calls mutex"))?;
        let call_idx = *calls;
        *calls += 1;
        drop(calls);

        match call_idx {
            0 => {
                assert_eq!(first_user_text(&prompt), Some("first input".to_string()));
                assert!(history.is_empty());
                Ok(Box::pin(stream::iter(vec![
                    Ok(SessionCompletionEvent::AssistantItem(
                        SessionAssistantContent::ReasoningDelta {
                            id: Some("r1".to_string()),
                            reasoning: "thinking-".to_string(),
                        },
                    )),
                    Ok(SessionCompletionEvent::AssistantItem(
                        SessionAssistantContent::ReasoningDelta {
                            id: Some("r1".to_string()),
                            reasoning: "part-".to_string(),
                        },
                    )),
                    Ok(SessionCompletionEvent::AssistantItem(
                        SessionAssistantContent::ReasoningDelta {
                            id: Some("r2".to_string()),
                            reasoning: "another".to_string(),
                        },
                    )),
                    Ok(SessionCompletionEvent::AssistantItem(
                        SessionAssistantContent::Text(Text {
                            text: "final-response".to_string(),
                        }),
                    )),
                    Ok(SessionCompletionEvent::Completed(SessionTurnSummary {
                        assistant_message_id: Some("assistant-final".to_string()),
                        response: "final-response".to_string(),
                    })),
                ])))
            }
            1 => {
                assert_eq!(
                    first_user_text(&prompt),
                    Some("follow up".to_string()),
                    "expected second user prompt"
                );
                assert_eq!(
                    history.len(),
                    2,
                    "expected first user + terminal assistant in history"
                );
                assert_eq!(
                    first_user_text(&history[0]),
                    Some("first input".to_string())
                );
                let (text, reasonings) = assistant_text_and_reasonings(&history[1]);
                assert_eq!(text.as_deref(), Some("final-response"));
                assert_eq!(
                    reasonings.len(),
                    2,
                    "expected both reasoning blocks preserved in terminal assistant message"
                );
                assert_eq!(reasonings[0].id.as_deref(), Some("r1"));
                assert_eq!(reasoning_concat_text(&reasonings[0]), "thinking-part-");
                assert_eq!(reasonings[1].id.as_deref(), Some("r2"));
                assert_eq!(reasoning_concat_text(&reasonings[1]), "another");
                Ok(Box::pin(stream::iter(vec![
                    Ok(SessionCompletionEvent::AssistantItem(
                        SessionAssistantContent::Text(Text {
                            text: "ack".to_string(),
                        }),
                    )),
                    Ok(SessionCompletionEvent::Completed(SessionTurnSummary {
                        assistant_message_id: Some("assistant-second".to_string()),
                        response: "ack".to_string(),
                    })),
                ])))
            }
            other => panic!("unexpected completion turn {other}"),
        }
    }
}

struct ReasoningToolLoopDriver {
    calls: Mutex<usize>,
}

impl SessionModelDriver for ReasoningToolLoopDriver {
    fn stream_completion_turn(
        &self,
        prompt: Message,
        history: Vec<Message>,
    ) -> Result<SessionCompletionStream> {
        let mut calls = self
            .calls
            .lock()
            .map_err(|_| anyhow::anyhow!("calls mutex"))?;
        let call_idx = *calls;
        *calls += 1;
        drop(calls);

        match call_idx {
            0 => {
                assert_eq!(first_user_text(&prompt), Some("tool loop".to_string()));
                assert!(history.is_empty());
                let tool_call = ToolCall::new(
                    "tool-1".to_string(),
                    ToolFunction::new(
                        "normal_tool".to_string(),
                        serde_json::json!({ "value": "ok" }),
                    ),
                )
                .with_call_id("call-1".to_string());
                Ok(Box::pin(stream::iter(vec![
                    Ok(SessionCompletionEvent::AssistantItem(
                        SessionAssistantContent::ReasoningDelta {
                            id: Some("r-pre".to_string()),
                            reasoning: "before-tool".to_string(),
                        },
                    )),
                    Ok(SessionCompletionEvent::AssistantItem(
                        SessionAssistantContent::ToolCall {
                            tool_call,
                            internal_call_id: "internal-call-1".to_string(),
                        },
                    )),
                    Ok(SessionCompletionEvent::Completed(SessionTurnSummary {
                        assistant_message_id: Some("assistant-tool-phase".to_string()),
                        response: String::new(),
                    })),
                ])))
            }
            1 => {
                assert_eq!(history.len(), 2);
                assert_eq!(first_user_text(&history[0]), Some("tool loop".to_string()));
                let (_, reasonings) = assistant_text_and_reasonings(&history[1]);
                assert_eq!(reasonings.len(), 1);
                assert_eq!(reasonings[0].id.as_deref(), Some("r-pre"));
                assert_eq!(reasoning_concat_text(&reasonings[0]), "before-tool");
                Ok(Box::pin(stream::iter(vec![
                    Ok(SessionCompletionEvent::AssistantItem(
                        SessionAssistantContent::ReasoningDelta {
                            id: Some("r-post".to_string()),
                            reasoning: "after-tool".to_string(),
                        },
                    )),
                    Ok(SessionCompletionEvent::AssistantItem(
                        SessionAssistantContent::Text(Text {
                            text: "tool-loop-done".to_string(),
                        }),
                    )),
                    Ok(SessionCompletionEvent::Completed(SessionTurnSummary {
                        assistant_message_id: Some("assistant-final".to_string()),
                        response: "tool-loop-done".to_string(),
                    })),
                ])))
            }
            2 => {
                assert_eq!(first_user_text(&prompt), Some("verify".to_string()));
                assert_eq!(
                    history.len(),
                    4,
                    "expected user/assistant(tool)/user(result)/assistant(terminal)"
                );
                let (_, r1) = assistant_text_and_reasonings(&history[1]);
                assert_eq!(r1.len(), 1);
                assert_eq!(r1[0].id.as_deref(), Some("r-pre"));
                assert_eq!(reasoning_concat_text(&r1[0]), "before-tool");
                let (text2, r2) = assistant_text_and_reasonings(&history[3]);
                assert_eq!(text2.as_deref(), Some("tool-loop-done"));
                assert_eq!(r2.len(), 1);
                assert_eq!(r2[0].id.as_deref(), Some("r-post"));
                assert_eq!(reasoning_concat_text(&r2[0]), "after-tool");
                Ok(Box::pin(stream::iter(vec![
                    Ok(SessionCompletionEvent::AssistantItem(
                        SessionAssistantContent::Text(Text {
                            text: "verified".to_string(),
                        }),
                    )),
                    Ok(SessionCompletionEvent::Completed(SessionTurnSummary {
                        assistant_message_id: Some("assistant-verify".to_string()),
                        response: "verified".to_string(),
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

struct ReasoningStreamFactory;

impl SessionModelFactory for ReasoningStreamFactory {
    fn build(
        &self,
        _cwd: PathBuf,
        _thread_id: ThreadId,
        _ask_user_client: Option<AskUserClient>,
        _current_turn_id: Arc<RwLock<Option<String>>>,
        _system_prompt_kind: SystemPromptKind,
        _agent_control: AgentControl,
        _plan_mode: bool,
    ) -> Result<SessionModel> {
        Ok(SessionModel::Stub(Arc::new(ReasoningStreamDriver {
            calls: Mutex::new(0),
        })))
    }
}

struct ReasoningToolLoopFactory;

impl SessionModelFactory for ReasoningToolLoopFactory {
    fn build(
        &self,
        _cwd: PathBuf,
        _thread_id: ThreadId,
        _ask_user_client: Option<AskUserClient>,
        _current_turn_id: Arc<RwLock<Option<String>>>,
        _system_prompt_kind: SystemPromptKind,
        _agent_control: AgentControl,
        _plan_mode: bool,
    ) -> Result<SessionModel> {
        Ok(SessionModel::Stub(Arc::new(ReasoningToolLoopDriver {
            calls: Mutex::new(0),
        })))
    }
}

struct PersistedToolLoopDriver {
    calls: Mutex<usize>,
}

impl SessionModelDriver for PersistedToolLoopDriver {
    fn stream_completion_turn(
        &self,
        prompt: Message,
        history: Vec<Message>,
    ) -> Result<SessionCompletionStream> {
        let mut calls = self
            .calls
            .lock()
            .map_err(|_| anyhow::anyhow!("calls mutex"))?;
        let call_idx = *calls;
        *calls += 1;
        drop(calls);

        match call_idx {
            0 => {
                assert_eq!(
                    first_user_text(&prompt),
                    Some("persisted tool loop".to_string())
                );
                assert!(history.is_empty());
                let tool_call = ToolCall::new(
                    "tool-1".to_string(),
                    ToolFunction::new(
                        "normal_tool".to_string(),
                        serde_json::json!({ "value": "ok" }),
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
                        assistant_message_id: Some("assistant-tool-phase".to_string()),
                        response: String::new(),
                    })),
                ])))
            }
            1 => {
                assert_eq!(history.len(), 2);
                assert_eq!(
                    first_user_text(&history[0]),
                    Some("persisted tool loop".to_string())
                );
                assert_eq!(assistant_tool_names(&history[1]), vec!["normal_tool"]);
                assert_eq!(tool_result_texts(&prompt), vec!["tool-output"]);
                Ok(Box::pin(stream::iter(vec![
                    Ok(SessionCompletionEvent::AssistantItem(
                        SessionAssistantContent::Text(Text {
                            text: "tool-loop-final".to_string(),
                        }),
                    )),
                    Ok(SessionCompletionEvent::Completed(SessionTurnSummary {
                        assistant_message_id: Some("assistant-final".to_string()),
                        response: "tool-loop-final".to_string(),
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

struct VerifyPersistedToolLoopDriver;

impl SessionModelDriver for VerifyPersistedToolLoopDriver {
    fn stream_completion_turn(
        &self,
        prompt: Message,
        history: Vec<Message>,
    ) -> Result<SessionCompletionStream> {
        assert_eq!(first_user_text(&prompt), Some("after resume".to_string()));
        assert_eq!(
            history.len(),
            4,
            "expected initial user, assistant tool call, user tool result, final assistant"
        );
        assert_eq!(
            first_user_text(&history[0]),
            Some("persisted tool loop".to_string())
        );
        assert_eq!(assistant_tool_names(&history[1]), vec!["normal_tool"]);
        assert_eq!(tool_result_texts(&history[2]), vec!["tool-output"]);
        let (text, reasonings) = assistant_text_and_reasonings(&history[3]);
        assert_eq!(text.as_deref(), Some("tool-loop-final"));
        assert!(reasonings.is_empty());

        Ok(Box::pin(stream::iter(vec![
            Ok(SessionCompletionEvent::AssistantItem(
                SessionAssistantContent::Text(Text {
                    text: "verified resume".to_string(),
                }),
            )),
            Ok(SessionCompletionEvent::Completed(SessionTurnSummary {
                assistant_message_id: Some("assistant-after-resume".to_string()),
                response: "verified resume".to_string(),
            })),
        ])))
    }
}

struct PersistedToolLoopFactory {
    build_count: Mutex<usize>,
}

impl SessionModelFactory for PersistedToolLoopFactory {
    fn build(
        &self,
        _cwd: PathBuf,
        _thread_id: ThreadId,
        _ask_user_client: Option<AskUserClient>,
        _current_turn_id: Arc<RwLock<Option<String>>>,
        _system_prompt_kind: SystemPromptKind,
        _agent_control: AgentControl,
        _plan_mode: bool,
    ) -> Result<SessionModel> {
        let mut build_count = self
            .build_count
            .lock()
            .map_err(|_| anyhow::anyhow!("build count mutex"))?;
        let model = if *build_count == 0 {
            SessionModel::Stub(Arc::new(PersistedToolLoopDriver {
                calls: Mutex::new(0),
            }))
        } else {
            SessionModel::Stub(Arc::new(VerifyPersistedToolLoopDriver))
        };
        *build_count += 1;
        Ok(model)
    }
}

/// Emits an Anthropic-shaped reasoning stream: idless `ReasoningDelta` chunks
/// followed by a single idless full `Reasoning` block carrying a signature.
/// Used to catch the regression where the pending idless delta bucket wasn't
/// cleared by the idless completion — the resulting duplicate unsigned
/// reasoning would be rejected by Anthropic on the next request.
struct IdlessReasoningCompletionDriver {
    calls: Mutex<usize>,
}

impl SessionModelDriver for IdlessReasoningCompletionDriver {
    fn stream_completion_turn(
        &self,
        prompt: Message,
        history: Vec<Message>,
    ) -> Result<SessionCompletionStream> {
        let mut calls = self
            .calls
            .lock()
            .map_err(|_| anyhow::anyhow!("calls mutex"))?;
        let call_idx = *calls;
        *calls += 1;
        drop(calls);

        match call_idx {
            0 => {
                assert_eq!(
                    first_user_text(&prompt),
                    Some("anthropic shape".to_string())
                );
                assert!(history.is_empty());
                let signed_completion =
                    Reasoning::new_with_signature("thinking", Some("sig-abc".to_string()));
                Ok(Box::pin(stream::iter(vec![
                    Ok(SessionCompletionEvent::AssistantItem(
                        SessionAssistantContent::ReasoningDelta {
                            id: None,
                            reasoning: "think".to_string(),
                        },
                    )),
                    Ok(SessionCompletionEvent::AssistantItem(
                        SessionAssistantContent::ReasoningDelta {
                            id: None,
                            reasoning: "ing".to_string(),
                        },
                    )),
                    Ok(SessionCompletionEvent::AssistantItem(
                        SessionAssistantContent::Reasoning(signed_completion),
                    )),
                    Ok(SessionCompletionEvent::AssistantItem(
                        SessionAssistantContent::Text(Text {
                            text: "final".to_string(),
                        }),
                    )),
                    Ok(SessionCompletionEvent::Completed(SessionTurnSummary {
                        assistant_message_id: Some("assistant-anthropic".to_string()),
                        response: "final".to_string(),
                    })),
                ])))
            }
            1 => {
                assert_eq!(first_user_text(&prompt), Some("follow up".to_string()));
                assert_eq!(history.len(), 2);
                let (text, reasonings) = assistant_text_and_reasonings(&history[1]);
                assert_eq!(text.as_deref(), Some("final"));
                assert_eq!(
                    reasonings.len(),
                    1,
                    "expected exactly one reasoning block (the signed completion), \
                     not a duplicate unsigned block from leftover deltas"
                );
                assert!(reasonings[0].id.is_none());
                let signed = reasonings[0].content.iter().find_map(|item| match item {
                    ReasoningContent::Text { text, signature } => {
                        Some((text.clone(), signature.clone()))
                    }
                    _ => None,
                });
                assert_eq!(
                    signed,
                    Some(("thinking".to_string(), Some("sig-abc".to_string()))),
                    "expected the signed completion content to be preserved verbatim"
                );
                Ok(Box::pin(stream::iter(vec![
                    Ok(SessionCompletionEvent::AssistantItem(
                        SessionAssistantContent::Text(Text {
                            text: "ack".to_string(),
                        }),
                    )),
                    Ok(SessionCompletionEvent::Completed(SessionTurnSummary {
                        assistant_message_id: Some("assistant-followup".to_string()),
                        response: "ack".to_string(),
                    })),
                ])))
            }
            other => panic!("unexpected completion turn {other}"),
        }
    }
}

struct IdlessReasoningCompletionFactory;

impl SessionModelFactory for IdlessReasoningCompletionFactory {
    fn build(
        &self,
        _cwd: PathBuf,
        _thread_id: ThreadId,
        _ask_user_client: Option<AskUserClient>,
        _current_turn_id: Arc<RwLock<Option<String>>>,
        _system_prompt_kind: SystemPromptKind,
        _agent_control: AgentControl,
        _plan_mode: bool,
    ) -> Result<SessionModel> {
        Ok(SessionModel::Stub(Arc::new(
            IdlessReasoningCompletionDriver {
                calls: Mutex::new(0),
            },
        )))
    }
}

/// Emits a `Reasoning` completion whose only content is encrypted bytes (the
/// OpenAI o-series / gpt-oss shape). Used to verify that blocks without
/// human-readable text are still preserved in history so they can be
/// roundtripped to the provider on the next turn.
struct EncryptedReasoningDriver {
    calls: Mutex<usize>,
}

impl SessionModelDriver for EncryptedReasoningDriver {
    fn stream_completion_turn(
        &self,
        prompt: Message,
        history: Vec<Message>,
    ) -> Result<SessionCompletionStream> {
        let mut calls = self
            .calls
            .lock()
            .map_err(|_| anyhow::anyhow!("calls mutex"))?;
        let call_idx = *calls;
        *calls += 1;
        drop(calls);

        match call_idx {
            0 => {
                assert_eq!(
                    first_user_text(&prompt),
                    Some("encrypted shape".to_string())
                );
                assert!(history.is_empty());
                // The Reasoning struct is `#[non_exhaustive]`, so we can't
                // construct it via a struct literal from outside rig. Build
                // it via the public constructor and replace its content vec
                // with an Encrypted block.
                let mut encrypted = Reasoning::new("");
                encrypted.id = Some("rs_enc".to_string());
                encrypted.content.clear();
                encrypted
                    .content
                    .push(ReasoningContent::Encrypted("opaque-cot-bytes".to_string()));
                Ok(Box::pin(stream::iter(vec![
                    Ok(SessionCompletionEvent::AssistantItem(
                        SessionAssistantContent::Reasoning(encrypted),
                    )),
                    Ok(SessionCompletionEvent::AssistantItem(
                        SessionAssistantContent::Text(Text {
                            text: "answer".to_string(),
                        }),
                    )),
                    Ok(SessionCompletionEvent::Completed(SessionTurnSummary {
                        assistant_message_id: Some("assistant-encrypted".to_string()),
                        response: "answer".to_string(),
                    })),
                ])))
            }
            1 => {
                assert_eq!(first_user_text(&prompt), Some("follow up".to_string()));
                assert_eq!(history.len(), 2);
                let (text, reasonings) = assistant_text_and_reasonings(&history[1]);
                assert_eq!(text.as_deref(), Some("answer"));
                assert_eq!(
                    reasonings.len(),
                    1,
                    "encrypted reasoning block should be preserved in history"
                );
                assert_eq!(reasonings[0].id.as_deref(), Some("rs_enc"));
                let encrypted_payload = reasonings[0].content.iter().find_map(|item| match item {
                    ReasoningContent::Encrypted(blob) => Some(blob.clone()),
                    _ => None,
                });
                assert_eq!(
                    encrypted_payload.as_deref(),
                    Some("opaque-cot-bytes"),
                    "expected encrypted payload to be roundtripped verbatim"
                );
                Ok(Box::pin(stream::iter(vec![
                    Ok(SessionCompletionEvent::AssistantItem(
                        SessionAssistantContent::Text(Text {
                            text: "ack".to_string(),
                        }),
                    )),
                    Ok(SessionCompletionEvent::Completed(SessionTurnSummary {
                        assistant_message_id: Some("assistant-followup".to_string()),
                        response: "ack".to_string(),
                    })),
                ])))
            }
            other => panic!("unexpected completion turn {other}"),
        }
    }
}

struct EncryptedReasoningFactory;

impl SessionModelFactory for EncryptedReasoningFactory {
    fn build(
        &self,
        _cwd: PathBuf,
        _thread_id: ThreadId,
        _ask_user_client: Option<AskUserClient>,
        _current_turn_id: Arc<RwLock<Option<String>>>,
        _system_prompt_kind: SystemPromptKind,
        _agent_control: AgentControl,
        _plan_mode: bool,
    ) -> Result<SessionModel> {
        Ok(SessionModel::Stub(Arc::new(EncryptedReasoningDriver {
            calls: Mutex::new(0),
        })))
    }
}

async fn wait_for_turn_completion(
    events: &mut tokio::sync::broadcast::Receiver<smooth_protocol::Event>,
    turn_id: &str,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let event = match tokio::time::timeout(remaining, events.recv()).await {
            Ok(Ok(event)) => event,
            Ok(Err(err)) => panic!("event channel closed: {err}"),
            Err(_) => panic!("timed out waiting for turn {turn_id} to complete"),
        };
        if let EventMsg::TurnCompleted(TurnCompletedEvent {
            turn_id: completed_id,
            ..
        }) = event.msg
            && completed_id == turn_id
        {
            return;
        }
    }
    panic!("turn {turn_id} did not complete in time");
}

#[tokio::test]
async fn terminal_turn_preserves_multi_id_reasoning_in_history() -> Result<()> {
    let _cwd_guard = CWD_LOCK.lock().map_err(|_| anyhow::anyhow!("cwd lock"))?;
    let workspace = TempDir::new()?;
    let original_cwd = std::env::current_dir()?;
    std::env::set_current_dir(workspace.path())?;

    let manager = ThreadManagerState::new(None, Some(Arc::new(ReasoningStreamFactory))).await?;
    let started = manager.start_thread().await?;
    let root_id = started.thread_id;
    let mut root_events = manager.subscribe(root_id).await?;

    let first_turn = manager
        .start_user_input(root_id, "first input".to_string())
        .await?;
    wait_for_turn_completion(&mut root_events, &first_turn).await;

    // Second turn triggers the driver's assertions on the persisted history.
    let second_turn = manager
        .start_user_input(root_id, "follow up".to_string())
        .await?;
    wait_for_turn_completion(&mut root_events, &second_turn).await;

    std::env::set_current_dir(original_cwd)?;
    Ok(())
}

#[tokio::test]
async fn reasoning_persists_across_tool_call_iteration_and_terminal_turn() -> Result<()> {
    let _cwd_guard = CWD_LOCK.lock().map_err(|_| anyhow::anyhow!("cwd lock"))?;
    let workspace = TempDir::new()?;
    let original_cwd = std::env::current_dir()?;
    std::env::set_current_dir(workspace.path())?;

    let manager = ThreadManagerState::new(None, Some(Arc::new(ReasoningToolLoopFactory))).await?;
    let started = manager.start_thread().await?;
    let root_id = started.thread_id;
    let mut root_events = manager.subscribe(root_id).await?;

    let first_turn = manager
        .start_user_input(root_id, "tool loop".to_string())
        .await?;
    wait_for_turn_completion(&mut root_events, &first_turn).await;

    let second_turn = manager
        .start_user_input(root_id, "verify".to_string())
        .await?;
    wait_for_turn_completion(&mut root_events, &second_turn).await;

    std::env::set_current_dir(original_cwd)?;
    Ok(())
}

#[tokio::test]
async fn resumed_completed_tool_loop_history_includes_tool_results() -> Result<()> {
    let _cwd_guard = CWD_LOCK.lock().map_err(|_| anyhow::anyhow!("cwd lock"))?;
    let workspace = TempDir::new()?;
    let original_cwd = std::env::current_dir()?;
    std::env::set_current_dir(workspace.path())?;

    let factory = Arc::new(PersistedToolLoopFactory {
        build_count: Mutex::new(0),
    });
    let manager = ThreadManagerState::new(None, Some(factory.clone())).await?;
    let started = manager.start_thread().await?;
    let root_id = started.thread_id;
    let mut root_events = manager.subscribe(root_id).await?;

    let first_turn = manager
        .start_user_input(root_id, "persisted tool loop".to_string())
        .await?;
    wait_for_turn_completion(&mut root_events, &first_turn).await;
    drop(manager);

    let resumed_manager = ThreadManagerState::new(None, Some(factory)).await?;
    let _resumed = resumed_manager.resume_thread(root_id).await?;
    let mut resumed_events = resumed_manager.subscribe(root_id).await?;
    let follow_up = resumed_manager
        .start_user_input(root_id, "after resume".to_string())
        .await?;
    wait_for_turn_completion(&mut resumed_events, &follow_up).await;

    std::env::set_current_dir(original_cwd)?;
    Ok(())
}

#[tokio::test]
async fn idless_reasoning_completion_supersedes_pending_deltas_without_duplicating() -> Result<()> {
    let _cwd_guard = CWD_LOCK.lock().map_err(|_| anyhow::anyhow!("cwd lock"))?;
    let workspace = TempDir::new()?;
    let original_cwd = std::env::current_dir()?;
    std::env::set_current_dir(workspace.path())?;

    let manager =
        ThreadManagerState::new(None, Some(Arc::new(IdlessReasoningCompletionFactory))).await?;
    let started = manager.start_thread().await?;
    let root_id = started.thread_id;
    let mut root_events = manager.subscribe(root_id).await?;

    let first_turn = manager
        .start_user_input(root_id, "anthropic shape".to_string())
        .await?;
    wait_for_turn_completion(&mut root_events, &first_turn).await;

    // The driver's call-1 arm asserts the persisted history contains exactly
    // one reasoning content item (the signed completion) — no leftover unsigned
    // duplicate from the pending delta bucket.
    let second_turn = manager
        .start_user_input(root_id, "follow up".to_string())
        .await?;
    wait_for_turn_completion(&mut root_events, &second_turn).await;

    std::env::set_current_dir(original_cwd)?;
    Ok(())
}

#[tokio::test]
async fn encrypted_reasoning_block_is_preserved_in_history() -> Result<()> {
    let _cwd_guard = CWD_LOCK.lock().map_err(|_| anyhow::anyhow!("cwd lock"))?;
    let workspace = TempDir::new()?;
    let original_cwd = std::env::current_dir()?;
    std::env::set_current_dir(workspace.path())?;

    let manager = ThreadManagerState::new(None, Some(Arc::new(EncryptedReasoningFactory))).await?;
    let started = manager.start_thread().await?;
    let root_id = started.thread_id;
    let mut root_events = manager.subscribe(root_id).await?;

    let first_turn = manager
        .start_user_input(root_id, "encrypted shape".to_string())
        .await?;
    wait_for_turn_completion(&mut root_events, &first_turn).await;

    // The driver's call-1 arm asserts the persisted history retained the
    // encrypted reasoning block verbatim. The fix at the manual-turn
    // Reasoning handler (gate on reasoning.content.is_empty() rather than
    // text.is_empty()) is what makes this hold — previously the block was
    // dropped because its human-readable text was empty.
    let second_turn = manager
        .start_user_input(root_id, "follow up".to_string())
        .await?;
    wait_for_turn_completion(&mut root_events, &second_turn).await;

    std::env::set_current_dir(original_cwd)?;
    Ok(())
}

struct SequentialSpawnParentDriver {
    calls: Mutex<usize>,
}

impl SessionModelDriver for SequentialSpawnParentDriver {
    fn stream_completion_turn(
        &self,
        _prompt: Message,
        _history: Vec<Message>,
    ) -> Result<SessionCompletionStream> {
        let mut calls = self
            .calls
            .lock()
            .map_err(|_| anyhow::anyhow!("calls mutex"))?;
        let idx = *calls;
        *calls += 1;
        drop(calls);

        // Even model calls spawn one child (a pure-spawn batch that blocks until
        // the child completes and is consumed); odd calls answer with final text
        // so the surrounding parent turn ends. Each user turn is therefore one
        // spawn + reclaim of a single child.
        if idx % 2 == 0 {
            let tool_call = ToolCall::new(
                format!("spawn-{idx}"),
                ToolFunction::new(
                    "spawn_agent".to_string(),
                    serde_json::json!({
                        "description": "sequential child",
                        "prompt": "do scoped work"
                    }),
                ),
            )
            .with_call_id(format!("call-{idx}"));
            Ok(Box::pin(stream::iter(vec![
                Ok(SessionCompletionEvent::AssistantItem(
                    SessionAssistantContent::ToolCall {
                        tool_call,
                        internal_call_id: format!("internal-{idx}"),
                    },
                )),
                Ok(SessionCompletionEvent::Completed(SessionTurnSummary {
                    assistant_message_id: Some(format!("assistant-spawn-{idx}")),
                    response: String::new(),
                })),
            ])))
        } else {
            Ok(Box::pin(stream::iter(vec![
                Ok(SessionCompletionEvent::AssistantItem(
                    SessionAssistantContent::Text(Text {
                        text: "parent done".to_string(),
                    }),
                )),
                Ok(SessionCompletionEvent::Completed(SessionTurnSummary {
                    assistant_message_id: Some(format!("assistant-final-{idx}")),
                    response: "parent done".to_string(),
                })),
            ])))
        }
    }
}

struct SequentialSpawnFactory;

impl SessionModelFactory for SequentialSpawnFactory {
    fn build(
        &self,
        _cwd: PathBuf,
        _thread_id: ThreadId,
        _ask_user_client: Option<AskUserClient>,
        _current_turn_id: Arc<RwLock<Option<String>>>,
        system_prompt_kind: SystemPromptKind,
        _agent_control: AgentControl,
        _plan_mode: bool,
    ) -> Result<SessionModel> {
        match system_prompt_kind {
            SystemPromptKind::Root => Ok(SessionModel::Stub(Arc::new(
                SequentialSpawnParentDriver {
                    calls: Mutex::new(0),
                },
            ))),
            _ => Ok(SessionModel::Stub(Arc::new(StubDriver {
                text: "child done".to_string(),
            }))),
        }
    }
}

/// Regression test for the spawn-budget leak: completed subagents used to stay
/// registered forever, so the `AGENT_MAX_THREADS` (16) cap silently became a
/// per-session *lifetime* budget — the 16th sequential spawn failed even though
/// only one child is ever live at a time. With consumed children reclaimed, a
/// root can spawn far more than the concurrency cap over its lifetime.
#[tokio::test]
async fn sequential_completed_subagents_do_not_exhaust_the_spawn_budget() -> Result<()> {
    let _cwd_guard = CWD_LOCK.lock().map_err(|_| anyhow::anyhow!("cwd lock"))?;
    let workspace = TempDir::new()?;
    let original_cwd = std::env::current_dir()?;
    std::env::set_current_dir(workspace.path())?;

    let manager = ThreadManagerState::new(None, Some(Arc::new(SequentialSpawnFactory))).await?;
    let started = manager.start_thread().await?;
    let root_id = started.thread_id;
    let mut root_events = manager.subscribe(root_id).await?;

    // Comfortably more than AGENT_MAX_THREADS (16).
    let total_children = 20;
    for _ in 0..total_children {
        let turn = manager
            .start_user_input(root_id, "spawn one".to_string())
            .await?;
        wait_for_turn_completion(&mut root_events, &turn).await;
    }

    let live = manager.list_agents(root_id, Some("/root"))?;
    assert_eq!(
        live.len(),
        1,
        "every consumed child should be reclaimed, leaving only root; got {live:?}"
    );

    std::env::set_current_dir(original_cwd)?;
    Ok(())
}

struct ConsumeThenBlockParentDriver {
    calls: Mutex<usize>,
    block: Arc<Semaphore>,
}

impl SessionModelDriver for ConsumeThenBlockParentDriver {
    fn stream_completion_turn(
        &self,
        _prompt: Message,
        _history: Vec<Message>,
    ) -> Result<SessionCompletionStream> {
        let mut calls = self
            .calls
            .lock()
            .map_err(|_| anyhow::anyhow!("calls mutex"))?;
        let idx = *calls;
        *calls += 1;
        drop(calls);

        if idx == 0 {
            // Spawn one child as a pure-spawn batch: the turn loop blocks until
            // the child completes, folds its result in, and releases it.
            let tool_call = ToolCall::new(
                "spawn-0".to_string(),
                ToolFunction::new(
                    "spawn_agent".to_string(),
                    serde_json::json!({
                        "description": "consumed child",
                        "prompt": "do scoped work"
                    }),
                ),
            )
            .with_call_id("call-0".to_string());
            Ok(Box::pin(stream::iter(vec![
                Ok(SessionCompletionEvent::AssistantItem(
                    SessionAssistantContent::ToolCall {
                        tool_call,
                        internal_call_id: "internal-0".to_string(),
                    },
                )),
                Ok(SessionCompletionEvent::Completed(SessionTurnSummary {
                    assistant_message_id: Some("assistant-spawn-0".to_string()),
                    response: String::new(),
                })),
            ])))
        } else {
            // Second model call blocks forever, holding the turn open *after* the
            // child was consumed/released but *before* the turn could persist its
            // result and close the child's edge. The test interrupts it here.
            let block = Arc::clone(&self.block);
            Ok(Box::pin(
                stream::once(async move {
                    block.acquire().await?.forget();
                    Ok(SessionCompletionEvent::AssistantItem(
                        SessionAssistantContent::Text(Text {
                            text: "unreached".to_string(),
                        }),
                    ))
                })
                .chain(stream::iter(vec![Ok(SessionCompletionEvent::Completed(
                    SessionTurnSummary {
                        assistant_message_id: Some("assistant-blocked".to_string()),
                        response: "unreached".to_string(),
                    },
                ))])),
            ))
        }
    }
}

struct ConsumeThenBlockFactory {
    block: Arc<Semaphore>,
}

impl SessionModelFactory for ConsumeThenBlockFactory {
    fn build(
        &self,
        _cwd: PathBuf,
        _thread_id: ThreadId,
        _ask_user_client: Option<AskUserClient>,
        _current_turn_id: Arc<RwLock<Option<String>>>,
        system_prompt_kind: SystemPromptKind,
        _agent_control: AgentControl,
        _plan_mode: bool,
    ) -> Result<SessionModel> {
        match system_prompt_kind {
            SystemPromptKind::Root => Ok(SessionModel::Stub(Arc::new(
                ConsumeThenBlockParentDriver {
                    calls: Mutex::new(0),
                    block: Arc::clone(&self.block),
                },
            ))),
            _ => Ok(SessionModel::Stub(Arc::new(StubDriver {
                text: "child done".to_string(),
            }))),
        }
    }
}

/// Regression test for the consume-before-persist window: a child consumed
/// mid-turn is released from memory immediately, but its durable parent→child
/// edge must stay open until the turn's result is persisted. If the turn is
/// interrupted before that, resume must still be able to rehydrate the child.
#[tokio::test]
async fn consumed_child_remains_rehydratable_after_midturn_interrupt() -> Result<()> {
    let _cwd_guard = CWD_LOCK.lock().map_err(|_| anyhow::anyhow!("cwd lock"))?;
    let workspace = TempDir::new()?;
    let original_cwd = std::env::current_dir()?;
    std::env::set_current_dir(workspace.path())?;

    let block = Arc::new(Semaphore::new(0));
    let manager = ThreadManagerState::new(
        None,
        Some(Arc::new(ConsumeThenBlockFactory {
            block: Arc::clone(&block),
        })),
    )
    .await?;
    let started = manager.start_thread().await?;
    let root_id = started.thread_id;
    let mut root_events = manager.subscribe(root_id).await?;
    let _turn = manager
        .start_user_input(root_id, "spawn then hang".to_string())
        .await?;

    // Wait until the child has completed (proving it ran).
    let mut child_completed = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, root_events.recv()).await {
            Ok(Ok(event)) => {
                if matches!(event.msg, EventMsg::CollabAgentCompleted(_)) {
                    child_completed = true;
                    break;
                }
            }
            Ok(Err(err)) => panic!("root event channel closed: {err}"),
            Err(_) => break,
        }
    }
    assert!(child_completed, "child should complete before the parent blocks");

    // Wait until the child has been released in-memory (registry back to just
    // root). The parent turn is now blocked in its second model call — past the
    // consume, before the end-of-turn edge close.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let live = manager.list_agents(root_id, Some("/root"))?;
        if live.len() == 1 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "consumed child was not released from the registry in time"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // Interrupt the parent mid-turn, before it could persist its result.
    manager.cancel_turn_subtree(root_id).await?;
    drop(manager);

    // Resume in a fresh manager. Because the edge was left open, the consumed
    // child is rehydrated rather than silently lost.
    let resumed_manager = ThreadManagerState::new(
        None,
        Some(Arc::new(ConsumeThenBlockFactory {
            block: Arc::new(Semaphore::new(0)),
        })),
    )
    .await?;
    let _resumed = resumed_manager.resume_thread(root_id).await?;
    let live = resumed_manager.list_agents(root_id, Some("/root"))?;
    assert_eq!(
        live.len(),
        2,
        "the open edge should let resume rehydrate the consumed child; got {live:?}"
    );

    std::env::set_current_dir(original_cwd)?;
    Ok(())
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
    assert_eq!(agent.event.as_deref(), Some("agent_status"));
    assert!(
        matches!(
            agent.status_detail,
            Some(AgentStatus::PendingInit) | Some(AgentStatus::Running)
        ),
        "expected a live child status, got {:?}",
        agent.status_detail
    );
    assert!(agent.last_assistant_message.is_none());
    assert_eq!(
        agent.next_action.as_deref(),
        Some("wait_for_agent_completed")
    );
    assert!(
        agent
            .instructions
            .as_deref()
            .is_some_and(|instructions| instructions.contains("No wait tool is needed"))
    );
    assert!(!agent.thread_id.is_empty());
    assert!(agent.agent_path.starts_with("/root/"));
}

fn assert_completed_spawn_result(agent: &TestAgentInfo) {
    assert_eq!(agent.event.as_deref(), Some("agent_completed"));
    assert!(
        matches!(agent.status_detail, Some(AgentStatus::Completed(_))),
        "expected a completed child status, got {:?}",
        agent.status_detail
    );
    assert_eq!(agent.status.as_deref(), Some("completed"));
    assert!(agent.last_assistant_message.is_some());
    assert_eq!(agent.next_action.as_deref(), Some("use_agent_result"));
    assert!(!agent.thread_id.is_empty());
    assert!(agent.agent_path.starts_with("/root/"));
}

fn tool_result_agent_infos(message: &Message) -> Vec<TestAgentInfo> {
    tool_result_texts(message)
        .into_iter()
        .map(|text| parse_agent_info(&text))
        .collect()
}

fn parse_agent_info(text: &str) -> TestAgentInfo {
    match serde_json::from_str(text) {
        Ok(info) => info,
        Err(err) => panic!("spawn_agent output json: {err}; payload={text}"),
    }
}

fn user_text_agent_info(message: &Message) -> TestAgentInfo {
    let Some(text) = first_user_text(message) else {
        panic!("user text spawn result");
    };
    parse_agent_info(&text)
}

fn user_text_agent_infos(message: &Message) -> Vec<TestAgentInfo> {
    match message {
        Message::User { content } => content
            .iter()
            .filter_map(|item| match item {
                UserContent::Text(text) => Some(parse_agent_info(&text.text)),
                _ => None,
            })
            .collect(),
        other => panic!("expected user text message, got {other:?}"),
    }
}

fn tool_result_texts(message: &Message) -> Vec<String> {
    match message {
        Message::User { content } => content
            .iter()
            .filter_map(|item| match item {
                UserContent::ToolResult(tool_result) => {
                    tool_result.content.iter().find_map(|item| match item {
                        rig::message::ToolResultContent::Text(text) => Some(text.text.clone()),
                        _ => None,
                    })
                }
                _ => None,
            })
            .collect(),
        other => panic!("expected tool result message, got {other:?}"),
    }
}

fn assistant_tool_names(message: &Message) -> Vec<String> {
    match message {
        Message::Assistant { content, .. } => content
            .iter()
            .filter_map(|item| match item {
                AssistantContent::ToolCall(tool_call) => Some(tool_call.function.name.clone()),
                _ => None,
            })
            .collect(),
        other => panic!("expected assistant tool-call message, got {other:?}"),
    }
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

fn assistant_text_and_reasonings(message: &Message) -> (Option<String>, Vec<Reasoning>) {
    match message {
        Message::Assistant { content, .. } => {
            let mut text = None;
            let mut reasonings = Vec::new();
            for item in content.iter() {
                match item {
                    AssistantContent::Text(t) => {
                        text = Some(t.text.clone());
                    }
                    AssistantContent::Reasoning(reasoning) => {
                        reasonings.push(reasoning.clone());
                    }
                    _ => {}
                }
            }
            (text, reasonings)
        }
        other => panic!("expected assistant message, got {other:?}"),
    }
}

fn reasoning_concat_text(reasoning: &Reasoning) -> String {
    reasoning
        .content
        .iter()
        .filter_map(|item| match item {
            ReasoningContent::Text { text, .. } | ReasoningContent::Summary(text) => {
                Some(text.as_str())
            }
            _ => None,
        })
        .collect()
}
