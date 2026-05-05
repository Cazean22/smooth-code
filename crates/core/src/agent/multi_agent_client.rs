use futures_util::future::BoxFuture;
use smooth_protocol::{
    AgentStatus, CollabAgentRef, CollabAgentSpawnBeginEvent, CollabAgentSpawnEndEvent,
    CollabAgentStatusEntry, CollabCloseBeginEvent, CollabCloseEndEvent,
    CollabSendMessageBeginEvent, CollabSendMessageEndEvent, CollabWaitingBeginEvent,
    CollabWaitingEndEvent, EventMsg, ThreadId,
};
use tools::{
    AgentInfo, AgentWaitOutcome, MultiAgentClient, SpawnAgentParams, ToolFailure, WaitAgentParams,
};
use uuid::Uuid;

use crate::agent::{AgentControl, registry::AgentMetadata, status::last_assistant_message};

#[derive(Clone)]
pub(crate) struct InProcessMultiAgentClient {
    author_thread_id: ThreadId,
    control: AgentControl,
}

impl InProcessMultiAgentClient {
    pub(crate) fn new(author_thread_id: ThreadId, control: AgentControl) -> Self {
        Self {
            author_thread_id,
            control,
        }
    }
}

impl MultiAgentClient for InProcessMultiAgentClient {
    fn spawn(
        &self,
        params: SpawnAgentParams,
    ) -> BoxFuture<'static, Result<AgentInfo, ToolFailure>> {
        let control = self.control.clone();
        let author_thread_id = self.author_thread_id;
        Box::pin(async move {
            let call_id = Uuid::now_v7().to_string();
            control
                .emit_collab_event(
                    author_thread_id,
                    EventMsg::CollabAgentSpawnBegin(CollabAgentSpawnBeginEvent {
                        call_id: call_id.clone(),
                        sender_thread_id: author_thread_id,
                        prompt: params.message.clone(),
                        model: params.model.clone(),
                    }),
                )
                .await;
            let result = control
                .spawn_agent_with_role(
                    author_thread_id,
                    params.message.clone(),
                    params.agent_type.clone(),
                    params.model.clone(),
                    params.fork_context,
                )
                .await;
            match result {
                Ok(metadata) => {
                    let thread_id = metadata
                        .agent_id
                        .expect("spawned agent should have a thread id");
                    let status = control.get_status(thread_id);
                    control
                        .emit_collab_event(
                            author_thread_id,
                            EventMsg::CollabAgentSpawnEnd(CollabAgentSpawnEndEvent {
                                call_id,
                                sender_thread_id: author_thread_id,
                                new_thread_id: Some(thread_id),
                                new_agent_nickname: metadata.agent_nickname.clone(),
                                new_agent_role: metadata.agent_role.clone(),
                                prompt: params.message,
                                model: params.model,
                                status: status.clone(),
                            }),
                        )
                        .await;
                    Ok(agent_info_from_metadata(&metadata, status))
                }
                Err(err) => {
                    control
                        .emit_collab_event(
                            author_thread_id,
                            EventMsg::CollabAgentSpawnEnd(CollabAgentSpawnEndEvent {
                                call_id,
                                sender_thread_id: author_thread_id,
                                new_thread_id: None,
                                new_agent_nickname: None,
                                new_agent_role: params.agent_type,
                                prompt: params.message,
                                model: params.model,
                                status: AgentStatus::Errored(err.to_string()),
                            }),
                        )
                        .await;
                    Err(ToolFailure::new(err.to_string()))
                }
            }
        })
    }

    fn send_message(
        &self,
        target: String,
        content: String,
        trigger_turn: bool,
    ) -> BoxFuture<'static, Result<String, ToolFailure>> {
        let control = self.control.clone();
        let author_thread_id = self.author_thread_id;
        Box::pin(async move {
            let call_id = Uuid::now_v7().to_string();
            let receiver_thread_id = control
                .resolve_agent_reference(author_thread_id, &target)
                .map_err(|err| ToolFailure::new(err.to_string()))?;
            control
                .emit_collab_event(
                    author_thread_id,
                    EventMsg::CollabSendMessageBegin(CollabSendMessageBeginEvent {
                        call_id: call_id.clone(),
                        sender_thread_id: author_thread_id,
                        receiver_thread_id,
                        prompt: content.clone(),
                    }),
                )
                .await;
            let result = control
                .send_input(author_thread_id, &target, content.clone(), trigger_turn)
                .await
                .map_err(|err| ToolFailure::new(err.to_string()))?;
            let receiver = control
                .registry()
                .agent_metadata_for_thread(receiver_thread_id)
                .ok_or_else(|| {
                    ToolFailure::new(format!(
                        "unknown live agent thread id: {receiver_thread_id}"
                    ))
                })?;
            control
                .emit_collab_event(
                    author_thread_id,
                    EventMsg::CollabSendMessageEnd(CollabSendMessageEndEvent {
                        call_id,
                        sender_thread_id: author_thread_id,
                        receiver_thread_id,
                        receiver_agent_nickname: receiver.agent_nickname,
                        receiver_agent_role: receiver.agent_role,
                        prompt: content,
                        status: control.get_status(receiver_thread_id),
                    }),
                )
                .await;
            Ok(result)
        })
    }

    fn wait_agent(
        &self,
        params: WaitAgentParams,
    ) -> BoxFuture<'static, Result<AgentWaitOutcome, ToolFailure>> {
        let control = self.control.clone();
        let author_thread_id = self.author_thread_id;
        Box::pin(async move {
            let call_id = Uuid::now_v7().to_string();
            let target_thread_id = control
                .resolve_agent_reference(author_thread_id, &params.target)
                .map_err(|err| ToolFailure::new(err.to_string()))?;
            let target_metadata = control
                .registry()
                .agent_metadata_for_thread(target_thread_id)
                .ok_or_else(|| {
                    ToolFailure::new(format!("unknown live agent thread id: {target_thread_id}"))
                })?;
            control
                .emit_collab_event(
                    author_thread_id,
                    EventMsg::CollabWaitingBegin(CollabWaitingBeginEvent {
                        sender_thread_id: author_thread_id,
                        receiver_thread_ids: vec![target_thread_id],
                        receiver_agents: vec![CollabAgentRef {
                            thread_id: target_thread_id,
                            agent_nickname: target_metadata.agent_nickname.clone(),
                            agent_role: target_metadata.agent_role.clone(),
                        }],
                        call_id: call_id.clone(),
                    }),
                )
                .await;
            let status = control
                .wait_for_agent(author_thread_id, &params.target, params.timeout_ms)
                .await
                .map_err(|err| ToolFailure::new(err.to_string()))?;
            control
                .emit_collab_event(
                    author_thread_id,
                    EventMsg::CollabWaitingEnd(CollabWaitingEndEvent {
                        sender_thread_id: author_thread_id,
                        call_id,
                        agent_statuses: vec![status_entry_from_metadata(
                            &target_metadata,
                            target_thread_id,
                            status.clone(),
                        )],
                        statuses: vec![status_entry_from_metadata(
                            &target_metadata,
                            target_thread_id,
                            status.clone(),
                        )],
                    }),
                )
                .await;
            Ok(AgentWaitOutcome {
                target: params.target,
                status: agent_status_label(&status),
                thread_id: target_thread_id.to_string(),
                agent_path: target_metadata.agent_path.to_string(),
                agent_nickname: target_metadata.agent_nickname,
                agent_role: target_metadata.agent_role,
                status_detail: status.clone(),
                last_assistant_message: last_assistant_message(&status),
            })
        })
    }

    fn list_agents(
        &self,
        path_prefix: Option<String>,
    ) -> BoxFuture<'static, Result<Vec<AgentInfo>, ToolFailure>> {
        let control = self.control.clone();
        let author_thread_id = self.author_thread_id;
        Box::pin(async move {
            control
                .list_agents(author_thread_id, path_prefix.as_deref())
                .map(|agents| {
                    agents
                        .into_iter()
                        .map(|agent| {
                            let status = agent
                                .agent_id
                                .map(|thread_id| control.get_status(thread_id))
                                .unwrap_or(AgentStatus::NotFound);
                            agent_info_from_metadata(&agent, status)
                        })
                        .collect()
                })
                .map_err(|err| ToolFailure::new(err.to_string()))
        })
    }

    fn close_agent(&self, target: String) -> BoxFuture<'static, Result<String, ToolFailure>> {
        let control = self.control.clone();
        let author_thread_id = self.author_thread_id;
        Box::pin(async move {
            let call_id = Uuid::now_v7().to_string();
            let receiver_thread_id = control
                .resolve_agent_reference(author_thread_id, &target)
                .map_err(|err| ToolFailure::new(err.to_string()))?;
            control
                .emit_collab_event(
                    author_thread_id,
                    EventMsg::CollabCloseBegin(CollabCloseBeginEvent {
                        call_id: call_id.clone(),
                        sender_thread_id: author_thread_id,
                        receiver_thread_id,
                    }),
                )
                .await;
            let receiver = control
                .registry()
                .agent_metadata_for_thread(receiver_thread_id)
                .ok_or_else(|| {
                    ToolFailure::new(format!(
                        "unknown live agent thread id: {receiver_thread_id}"
                    ))
                })?;
            let status = control
                .close_agent(author_thread_id, &target)
                .await
                .map_err(|err| ToolFailure::new(err.to_string()))?;
            control
                .emit_collab_event(
                    author_thread_id,
                    EventMsg::CollabCloseEnd(CollabCloseEndEvent {
                        call_id,
                        sender_thread_id: author_thread_id,
                        receiver_thread_id,
                        receiver_agent_nickname: receiver.agent_nickname,
                        receiver_agent_role: receiver.agent_role,
                        status: status.clone(),
                    }),
                )
                .await;
            Ok(agent_status_label(&status))
        })
    }
}

fn agent_info_from_metadata(metadata: &AgentMetadata, status: AgentStatus) -> AgentInfo {
    AgentInfo {
        thread_id: metadata
            .agent_id
            .map(|thread_id| thread_id.to_string())
            .unwrap_or_default(),
        agent_path: metadata.agent_path.to_string(),
        agent_nickname: metadata.agent_nickname.clone(),
        agent_role: metadata.agent_role.clone(),
        status: Some(agent_status_label(&status)),
        status_detail: Some(status.clone()),
        last_assistant_message: last_assistant_message(&status),
    }
}

fn status_entry_from_metadata(
    metadata: &AgentMetadata,
    thread_id: ThreadId,
    status: AgentStatus,
) -> CollabAgentStatusEntry {
    CollabAgentStatusEntry {
        thread_id,
        agent_path: metadata.agent_path.clone(),
        agent_nickname: metadata.agent_nickname.clone(),
        agent_role: metadata.agent_role.clone(),
        last_assistant_message: last_assistant_message(&status),
        status,
    }
}

fn agent_status_label(status: &AgentStatus) -> String {
    match status {
        AgentStatus::PendingInit => "pending_init".to_string(),
        AgentStatus::Running => "running".to_string(),
        AgentStatus::Completed(_) => "completed".to_string(),
        AgentStatus::Interrupted => "interrupted".to_string(),
        AgentStatus::Errored(_) => "errored".to_string(),
        AgentStatus::Shutdown => "shutdown".to_string(),
        AgentStatus::NotFound => "not_found".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, sync::Arc, time::Duration};

    use anyhow::Result;
    use rig::{
        agent::FinalResponse,
        message::{Message, Text},
    };
    use smooth_protocol::{AgentStatus, ThreadId};
    use tempfile::TempDir;
    use tokio::{sync::watch, time::sleep};
    use tools::{MultiAgentClient, SpawnAgentParams, WaitAgentParams};

    use crate::{
        SessionAssistantContent, SessionModel, SessionModelDriver, SessionModelFactory,
        SessionStream, agent::role::RoleOverride, provider::SessionStreamEvent,
        thread_manager::ThreadManagerState,
    };

    use super::InProcessMultiAgentClient;

    struct StubDriver {
        text: String,
        delay: Duration,
    }

    impl SessionModelDriver for StubDriver {
        fn stream_turn(&self, prompt: Message, history: Vec<Message>) -> Result<SessionStream> {
            let _ = (prompt, history);
            let text = self.text.clone();
            let delay = self.delay;
            Ok(Box::pin(async_stream::stream! {
                if !delay.is_zero() {
                    sleep(delay).await;
                }
                yield Ok(SessionStreamEvent::StreamAssistantItem(SessionAssistantContent::Text(Text { text })));
                yield Ok(SessionStreamEvent::FinalResponse(FinalResponse::empty()));
            }))
        }
    }

    struct StubFactory {
        model: SessionModel,
    }

    impl SessionModelFactory for StubFactory {
        fn build(
            &self,
            _cwd: PathBuf,
            _thread_id: ThreadId,
            _dynamic_tool_client: Option<Arc<dyn tools::DynamicToolClient>>,
            _current_turn_id: Arc<watch::Sender<Option<String>>>,
            _role_override: RoleOverride,
            _agent_control: crate::agent::AgentControl,
        ) -> Result<SessionModel> {
            Ok(self.model.clone())
        }
    }

    #[tokio::test]
    async fn adapter_spawn_and_list_agents_round_trip() {
        let _cwd_guard = crate::test_support::cwd_test_lock()
            .lock()
            .expect("cwd lock");
        let workspace = TempDir::new().expect("tempdir");
        let original_cwd = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(workspace.path()).expect("set cwd");

        let manager = ThreadManagerState::new(
            None,
            Some(Arc::new(StubFactory {
                model: SessionModel::Stub(Arc::new(StubDriver {
                    text: "child".to_string(),
                    delay: Duration::ZERO,
                })),
            })),
        )
        .await
        .expect("thread manager");
        let started = manager.start_thread().await.expect("start root");
        let client = InProcessMultiAgentClient::new(started.thread_id, manager.agent_control());

        let spawned = client
            .spawn(SpawnAgentParams {
                message: "inspect".to_string(),
                agent_type: Some("explorer".to_string()),
                model: None,
                fork_context: false,
            })
            .await
            .expect("spawn should succeed");
        assert!(spawned.agent_path.starts_with("/root/"));
        assert_eq!(spawned.agent_role.as_deref(), Some("explorer"));

        let listed = client
            .list_agents(Some("/root".to_string()))
            .await
            .expect("list should succeed");
        assert!(
            listed
                .iter()
                .any(|agent| agent.agent_path == spawned.agent_path)
        );

        std::env::set_current_dir(original_cwd).expect("restore cwd");
    }

    #[tokio::test]
    async fn adapter_wait_agent_returns_terminal_status() {
        let _cwd_guard = crate::test_support::cwd_test_lock()
            .lock()
            .expect("cwd lock");
        let workspace = TempDir::new().expect("tempdir");
        let original_cwd = std::env::current_dir().expect("cwd");
        std::env::set_current_dir(workspace.path()).expect("set cwd");

        let manager = ThreadManagerState::new(
            None,
            Some(Arc::new(StubFactory {
                model: SessionModel::Stub(Arc::new(StubDriver {
                    text: "child".to_string(),
                    delay: Duration::from_millis(20),
                })),
            })),
        )
        .await
        .expect("thread manager");
        let started = manager.start_thread().await.expect("start root");
        let client = InProcessMultiAgentClient::new(started.thread_id, manager.agent_control());
        let spawned = client
            .spawn(SpawnAgentParams {
                message: "inspect".to_string(),
                agent_type: Some("worker".to_string()),
                model: None,
                fork_context: false,
            })
            .await
            .expect("spawn should succeed");

        let waited = client
            .wait_agent(WaitAgentParams {
                target: spawned.agent_path,
                timeout_ms: Some(250),
            })
            .await
            .expect("wait should succeed");
        assert_eq!(waited.status, "completed");
        assert_eq!(
            waited.status_detail,
            AgentStatus::Completed(Some("child".to_string()))
        );
        assert_eq!(waited.last_assistant_message.as_deref(), Some("child"));

        std::env::set_current_dir(original_cwd).expect("restore cwd");
    }
}
