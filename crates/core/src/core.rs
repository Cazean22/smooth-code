use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use anyhow::Result;
use futures_util::future::BoxFuture;
use rig::{
    OneOrMany,
    message::{Message, Text, UserContent},
};
use smooth_protocol::{
    AgentStatus, AgentStatusChangedEvent, Event, EventMsg, Op, SessionSource, ThreadId,
    TurnCompletedEvent, TurnInterruptedEvent, TurnStartedEvent,
};
use tokio::sync::{Mutex, RwLock, broadcast};
use tools::DynamicToolClient;
use tracing::Instrument;

use crate::{
    agent::AgentControl,
    context_manager::ContextManager,
    provider::SessionModel,
    rollout::{HistoryMessage, PersistedItem, RolloutRecorder, persist_event},
    state::{ActiveTurn, RunningTask, SessionState},
    tasks::{AnySessionTask, RegularTask},
};

const EVENT_CHANNEL_CAPACITY: usize = 256;

pub struct Core {
    pub(crate) session: Arc<Session>,
}

/// A session has at most 1 running task at a time.
pub(crate) struct Session {
    pub(crate) id: ThreadId,
    event_tx: broadcast::Sender<Event>,
    state: Mutex<SessionState>,
    pub(crate) active_turn: Mutex<Option<ActiveTurn>>,
    #[allow(dead_code)]
    pub(crate) session_source: SessionSource,
    pub(crate) agent_control: AgentControl,
    current_turn_id: Arc<RwLock<Option<String>>>,
    dynamic_tool_client: Option<Arc<dyn DynamicToolClient>>,
    next_internal_sub_id: AtomicU64,
    model: SessionModel,
    rollout: RolloutRecorder,
}

/// The context needed for a single turn of the thread.
#[derive(Debug)]
pub(crate) struct TurnContext {
    pub(crate) sub_id: String,
    pub(crate) assistant_item_id: String,
    #[allow(dead_code)]
    pub(crate) timezone: Option<String>,
}

impl Core {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        id: ThreadId,
        model: SessionModel,
        history: Vec<Message>,
        next_internal_sub_id: u64,
        rollout: RolloutRecorder,
        current_turn_id: Arc<RwLock<Option<String>>>,
        dynamic_tool_client: Option<Arc<dyn DynamicToolClient>>,
        session_source: SessionSource,
        agent_control: AgentControl,
    ) -> Self {
        let (event_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let mut context_manager = ContextManager::default();
        context_manager.replace(history);
        let session = Arc::new(Session {
            id,
            event_tx,
            state: Mutex::new(SessionState::new(context_manager)),
            active_turn: Mutex::new(None),
            session_source,
            agent_control,
            current_turn_id,
            dynamic_tool_client,
            next_internal_sub_id: AtomicU64::new(next_internal_sub_id),
            model,
            rollout,
        });
        Self { session }
    }

    pub async fn start_user_input(&self, input: String) -> Result<String> {
        self.submit(Op::UserInput(input)).await
    }

    pub async fn submit(&self, op: Op) -> Result<String> {
        match op {
            Op::UserInput(input) => {
                let sub_id = self.session.start_regular_turn(vec![input]).await;
                Ok(sub_id)
            }
            Op::Interrupt => {
                self.session.abort_all_tasks("interrupted").await;
                self.session
                    .set_agent_status(AgentStatus::Interrupted, None)
                    .await;
                Ok("interrupted".to_string())
            }
            Op::Shutdown => {
                self.session.abort_all_tasks("shutdown").await;
                self.session
                    .set_agent_status(AgentStatus::Shutdown, None)
                    .await;
                Ok("shutdown".to_string())
            }
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.session.event_tx.subscribe()
    }

    pub(crate) async fn emit_session_event(&self, msg: EventMsg) {
        self.session.emit_session_event(msg).await;
    }

    pub(crate) async fn flush_rollout(&self) -> Result<()> {
        self.session.rollout.flush().await
    }
}

impl Session {
    fn start_regular_turn(self: &Arc<Self>, input: Vec<String>) -> BoxFuture<'_, String> {
        Box::pin(async move {
            let turn_context = Arc::new(self.fresh_turn_context());
            let sub_id = turn_context.sub_id.clone();
            let input_len = input.iter().map(String::len).sum::<usize>();
            tracing::debug!(
                thread_id = %self.id,
                turn_id = %sub_id,
                input_len,
                "starting turn"
            );
            self.start_task(
                turn_context,
                input,
                Arc::new(RegularTask::new()),
                crate::state::TaskKind::Regular,
            )
            .await;
            sub_id
        })
    }

    pub(crate) fn start_task(
        self: &Arc<Self>,
        turn_context: Arc<TurnContext>,
        input: Vec<String>,
        task: Arc<dyn AnySessionTask>,
        task_kind: crate::state::TaskKind,
    ) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            self.abort_all_tasks("replaced").await;
            let cancellation_token = tokio_util::sync::CancellationToken::new();
            let done = Arc::new(tokio::sync::Notify::new());
            let sess = Arc::clone(self);
            let task_for_runner = Arc::clone(&task);
            let ctx_for_runner = Arc::clone(&turn_context);
            let done_for_runner = Arc::clone(&done);
            let cancellation_for_runner = cancellation_token.clone();

            self.emit_event(
                &turn_context,
                EventMsg::TurnStarted(TurnStartedEvent {
                    thread_id: self.id.to_string(),
                    turn_id: turn_context.sub_id.clone(),
                }),
            )
            .await;
            *self.current_turn_id.write().await = Some(turn_context.sub_id.clone());
            self.set_agent_status(AgentStatus::Running, Some(turn_context.as_ref()))
                .await;

            let task_span = tracing::info_span!(
                "core.session_task",
                thread_id = %self.id,
                turn_id = %turn_context.sub_id,
                task_name = task_for_runner.span_name(),
                task_kind = ?task_for_runner.kind(),
            );
            let handle = tokio::task::Builder::new()
                .name(task_for_runner.span_name())
                .spawn(
                    async move {
                        let result = task_for_runner
                            .run(
                                Arc::clone(&sess),
                                Arc::clone(&ctx_for_runner),
                                input,
                                cancellation_for_runner.clone(),
                            )
                            .await;

                        if cancellation_for_runner.is_cancelled() {
                            sess.set_agent_status(
                                AgentStatus::Interrupted,
                                Some(ctx_for_runner.as_ref()),
                            )
                            .await;
                            sess.emit_event(
                                &ctx_for_runner,
                                EventMsg::TurnInterrupted(TurnInterruptedEvent {
                                    thread_id: sess.id.to_string(),
                                    turn_id: ctx_for_runner.sub_id.clone(),
                                    reason: "cancelled".to_string(),
                                }),
                            )
                            .await;
                        } else if let Some(last_assistant_message) = result {
                            sess.set_agent_status(
                                AgentStatus::Completed(Some(last_assistant_message.clone())),
                                Some(ctx_for_runner.as_ref()),
                            )
                            .await;
                            sess.emit_event(
                                &ctx_for_runner,
                                EventMsg::TurnCompleted(TurnCompletedEvent {
                                    thread_id: sess.id.to_string(),
                                    turn_id: ctx_for_runner.sub_id.clone(),
                                    last_assistant_message: Some(last_assistant_message),
                                }),
                            )
                            .await;
                        } else {
                            // The session task returned `None` without being
                            // cancelled. The driver may have already published
                            // a terminal status (e.g. `Errored` from a failed
                            // provider stream); in that case leave it alone so
                            // the cause survives. Only if the status is still
                            // non-terminal do we publish `Completed(None)` so
                            // any parent waiting on this thread's completion
                            // (`InlineChildCompletionReceiver`, the per-child
                            // status watcher in `AgentControl`) unblocks
                            // instead of stalling on `Running` forever.
                            let current_status = sess.agent_control.get_status(sess.id);
                            if crate::agent::status::is_final(&current_status) {
                                tracing::debug!(
                                    thread_id = %sess.id,
                                    turn_id = %ctx_for_runner.sub_id,
                                    status = ?current_status,
                                    "session task ended without a result; terminal status already set, leaving as-is"
                                );
                            } else {
                                tracing::warn!(
                                    thread_id = %sess.id,
                                    turn_id = %ctx_for_runner.sub_id,
                                    "session task ended without a result; marking turn completed with no assistant message"
                                );
                                sess.set_agent_status(
                                    AgentStatus::Completed(None),
                                    Some(ctx_for_runner.as_ref()),
                                )
                                .await;
                                sess.emit_event(
                                    &ctx_for_runner,
                                    EventMsg::TurnCompleted(TurnCompletedEvent {
                                        thread_id: sess.id.to_string(),
                                        turn_id: ctx_for_runner.sub_id.clone(),
                                        last_assistant_message: None,
                                    }),
                                )
                                .await;
                            }
                        }

                        let mut active_turn = sess.active_turn.lock().await;
                        if let Some(turn) = active_turn.as_mut()
                            && turn.remove_task(&ctx_for_runner.sub_id)
                        {
                            *active_turn = None;
                            *sess.current_turn_id.write().await = None;
                        }
                        drop(active_turn);

                        done_for_runner.notify_waiters();
                    }
                    .instrument(task_span),
                )
                .expect("failed to spawn session task");

            let running_task = RunningTask {
                done,
                kind: task_kind,
                task,
                cancellation_token,
                handle: Arc::new(tokio_util::task::AbortOnDropHandle::new(handle)),
                turn_context: Arc::clone(&turn_context),
            };

            let mut active_turn = self.active_turn.lock().await;
            let turn = active_turn.get_or_insert_with(ActiveTurn::default);
            turn.add_task(running_task);
        })
    }

    #[tracing::instrument(name = "core.session.abort_all_tasks", skip(self), fields(thread_id = %self.id, reason))]
    pub(crate) async fn abort_all_tasks(self: &Arc<Self>, reason: &str) {
        let drained = {
            let mut active_turn = self.active_turn.lock().await;
            active_turn.take().map(|mut turn| turn.drain_tasks())
        };

        if let Some(tasks) = drained {
            *self.current_turn_id.write().await = None;
            for task in tasks {
                task.cancellation_token.cancel();
                task.task
                    .abort(Arc::clone(self), Arc::clone(&task.turn_context))
                    .await;
                self.emit_event(
                    task.turn_context.as_ref(),
                    EventMsg::TurnInterrupted(TurnInterruptedEvent {
                        thread_id: self.id.to_string(),
                        turn_id: task.turn_context.sub_id.clone(),
                        reason: reason.to_string(),
                    }),
                )
                .await;
            }
        }
    }

    pub(crate) async fn history(&self) -> Vec<Message> {
        let state = self.state.lock().await;
        state.history.items().to_vec()
    }

    pub(crate) async fn replace_history(&self, history: Vec<Message>) {
        let mut state = self.state.lock().await;
        state.history.replace(history);
    }

    pub(crate) async fn record_user_message(&self, text: String) {
        let mut state = self.state.lock().await;
        state.history.push(Message::User {
            content: OneOrMany::one(UserContent::Text(Text { text: text.clone() })),
        });
        drop(state);
        let _ = self
            .rollout
            .append(PersistedItem::HistoryMessage(HistoryMessage::UserText {
                text,
            }))
            .await;
    }

    pub(crate) async fn persist_assistant_message(&self, text: String) {
        let _ = self
            .rollout
            .append(PersistedItem::HistoryMessage(
                HistoryMessage::AssistantText { text },
            ))
            .await;
    }

    pub(crate) async fn emit_event(&self, ctx: &TurnContext, msg: EventMsg) {
        if persist_event(&msg) {
            let _ = self.rollout.append(PersistedItem::Event(msg.clone())).await;
        }
        let _ = self.event_tx.send(Event {
            id: ctx.sub_id.clone(),
            msg,
        });
    }

    pub(crate) async fn emit_session_event(&self, msg: EventMsg) {
        if persist_event(&msg) {
            let _ = self.rollout.append(PersistedItem::Event(msg.clone())).await;
        }
        let _ = self.event_tx.send(Event {
            id: "session".to_string(),
            msg,
        });
    }

    pub(crate) async fn set_agent_status(&self, status: AgentStatus, ctx: Option<&TurnContext>) {
        self.agent_control.set_status(self.id, status.clone());
        let _ = self.event_tx.send(Event {
            id: ctx
                .map(|ctx| ctx.sub_id.clone())
                .unwrap_or_else(|| "status".to_string()),
            msg: EventMsg::AgentStatusChanged(AgentStatusChangedEvent {
                thread_id: self.id.to_string(),
                turn_id: ctx.map(|ctx| ctx.sub_id.clone()),
                status,
            }),
        });
    }

    pub(crate) fn model(&self) -> &SessionModel {
        &self.model
    }

    pub(crate) async fn abort_pending_dynamic_tool_requests(&self) {
        if let Some(dynamic_tool_client) = &self.dynamic_tool_client {
            // The transport adapter currently tracks pending dynamic-tool requests by thread.
            dynamic_tool_client.abort_pending_server_requests().await;
        }
    }

    pub(crate) fn fresh_turn_context(&self) -> TurnContext {
        let sub_id = self.next_internal_sub_id();
        TurnContext {
            assistant_item_id: format!("{sub_id}-assistant"),
            sub_id,
            timezone: None,
        }
    }

    fn next_internal_sub_id(&self) -> String {
        self.next_internal_sub_id
            .fetch_add(1, Ordering::Relaxed)
            .to_string()
    }
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, sync::Arc};

    use anyhow::Result;
    use rig::message::Message;
    use tempfile::TempDir;
    use tokio::sync::RwLock;

    use crate::{
        SessionModel, SessionModelDriver, SessionStream, agent::AgentControl,
        rollout::RolloutRecorder,
    };

    use super::Core;
    use smooth_protocol::{AgentStatusChangedEvent, EventMsg, SessionSource};

    struct EmptyDriver;

    impl SessionModelDriver for EmptyDriver {
        fn stream_turn(&self, _prompt: Message, _history: Vec<Message>) -> Result<SessionStream> {
            Ok(Box::pin(futures_util::stream::empty()))
        }
    }

    async fn test_core() -> (
        Core,
        tokio::sync::broadcast::Receiver<smooth_protocol::Event>,
    ) {
        let workspace = TempDir::new().expect("tempdir");
        let cwd = PathBuf::from(workspace.path());
        let thread_id = smooth_protocol::ThreadId::new();
        let current_turn_id = Arc::new(RwLock::new(None));
        let rollout = RolloutRecorder::create(workspace.path(), thread_id, &cwd)
            .await
            .expect("rollout");
        let core = Core::new(
            thread_id,
            SessionModel::Stub(Arc::new(EmptyDriver)),
            Vec::new(),
            0,
            rollout,
            current_turn_id,
            None,
            SessionSource::Cli,
            AgentControl::new(),
        );
        let rx = core.subscribe();
        (core, rx)
    }

    #[tokio::test]
    async fn submit_interrupt_emits_interrupted_status() {
        let (core, mut rx) = test_core().await;

        let result = core
            .submit(smooth_protocol::Op::Interrupt)
            .await
            .expect("interrupt submit");
        assert_eq!(result, "interrupted");

        let event = rx.recv().await.expect("status event");
        assert_eq!(
            event.msg,
            EventMsg::AgentStatusChanged(AgentStatusChangedEvent {
                thread_id: core.session.id.to_string(),
                turn_id: None,
                status: smooth_protocol::AgentStatus::Interrupted,
            })
        );
    }

    #[tokio::test]
    async fn submit_shutdown_emits_shutdown_status() {
        let (core, mut rx) = test_core().await;

        let result = core
            .submit(smooth_protocol::Op::Shutdown)
            .await
            .expect("shutdown submit");
        assert_eq!(result, "shutdown");

        let event = rx.recv().await.expect("status event");
        assert_eq!(
            event.msg,
            EventMsg::AgentStatusChanged(AgentStatusChangedEvent {
                thread_id: core.session.id.to_string(),
                turn_id: None,
                status: smooth_protocol::AgentStatus::Shutdown,
            })
        );
    }
}
