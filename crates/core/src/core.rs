use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use anyhow::Result;
use rig::{
    OneOrMany,
    message::{Message, Text, UserContent},
};
use smooth_protocol::{
    AgentStatus, AgentStatusChangedEvent, Event, EventMsg, ThreadId, TurnCompletedEvent,
    TurnInterruptedEvent, TurnStartedEvent,
};
use tokio::sync::{Mutex, broadcast, watch};
use tracing::Instrument;

use crate::{
    context_manager::ContextManager,
    provider::SessionModel,
    rollout::{HistoryMessage, PersistedItem, RolloutRecorder, persist_event},
    state::{ActiveTurn, RunningTask, SessionState},
    tasks::{RegularTask, SessionTask},
    tools::DynamicToolClient,
};

const EVENT_CHANNEL_CAPACITY: usize = 256;

pub struct Core {
    pub(crate) session: Arc<Session>,
}

/// A session has at most 1 running task at a time.
pub(crate) struct Session {
    pub(crate) id: ThreadId,
    agent_status: watch::Sender<AgentStatus>,
    event_tx: broadcast::Sender<Event>,
    state: Mutex<SessionState>,
    pub(crate) active_turn: Mutex<Option<ActiveTurn>>,
    current_turn_id: Arc<watch::Sender<Option<String>>>,
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
    pub(crate) timezone: Option<String>,
}

impl Core {
    pub(crate) fn new(
        id: ThreadId,
        model: SessionModel,
        history: Vec<Message>,
        next_internal_sub_id: u64,
        rollout: RolloutRecorder,
        current_turn_id: Arc<watch::Sender<Option<String>>>,
        dynamic_tool_client: Option<Arc<dyn DynamicToolClient>>,
    ) -> Self {
        let (agent_status, _) = watch::channel(AgentStatus::PendingInit);
        let (event_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let mut context_manager = ContextManager::default();
        context_manager.replace(history);
        let session = Arc::new(Session {
            id,
            agent_status,
            event_tx,
            state: Mutex::new(SessionState::new(context_manager)),
            active_turn: Mutex::new(None),
            current_turn_id,
            dynamic_tool_client,
            next_internal_sub_id: AtomicU64::new(next_internal_sub_id),
            model,
            rollout,
        });
        Self { session }
    }

    pub async fn start_user_input(&self, input: String) -> Result<String> {
        let sub_id = self.session.next_internal_sub_id();
        tracing::debug!(
            thread_id = %self.session.id,
            turn_id = %sub_id,
            input_len = input.len(),
            "starting turn"
        );
        let turn_context = Arc::new(TurnContext {
            assistant_item_id: format!("{sub_id}-assistant"),
            sub_id: sub_id.clone(),
            timezone: None,
        });

        self.session
            .start_task(turn_context, vec![input], RegularTask::new())
            .await;
        Ok(sub_id)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.session.event_tx.subscribe()
    }

    pub(crate) async fn emit_session_event(&self, msg: EventMsg) {
        self.session.emit_session_event(msg).await;
    }
}

impl Session {
    pub(crate) async fn start_task<T: SessionTask>(
        self: &Arc<Self>,
        turn_context: Arc<TurnContext>,
        input: Vec<String>,
        task: T,
    ) {
        self.abort_all_tasks("replaced").await;

        let task: Arc<dyn crate::tasks::AnySessionTask> = Arc::new(task);
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
        self.current_turn_id
            .send_replace(Some(turn_context.sub_id.clone()));
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
                    }

                    let mut active_turn = sess.active_turn.lock().await;
                    if let Some(turn) = active_turn.as_mut()
                        && turn.remove_task(&ctx_for_runner.sub_id)
                    {
                        // smooth-code currently runs a single task per turn, so once that task
                        // completes there is no longer a current turn to attribute dynamic tools to.
                        *active_turn = None;
                        sess.current_turn_id.send_replace(None);
                    }

                    done_for_runner.notify_waiters();
                }
                .instrument(task_span),
            )
            .expect("failed to spawn session task");

        let running_task = RunningTask {
            done,
            kind: task.kind(),
            task,
            cancellation_token,
            handle: Arc::new(tokio_util::task::AbortOnDropHandle::new(handle)),
            turn_context: Arc::clone(&turn_context),
        };

        let mut active_turn = self.active_turn.lock().await;
        let turn = active_turn.get_or_insert_with(ActiveTurn::default);
        turn.add_task(running_task);
    }

    #[tracing::instrument(name = "core.session.abort_all_tasks", skip(self), fields(thread_id = %self.id, reason))]
    pub(crate) async fn abort_all_tasks(self: &Arc<Self>, reason: &str) {
        let drained = {
            let mut active_turn = self.active_turn.lock().await;
            active_turn.take().map(|mut turn| turn.drain_tasks())
        };

        if let Some(tasks) = drained {
            self.current_turn_id.send_replace(None);
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
        self.agent_status.send_replace(status.clone());
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

    fn next_internal_sub_id(&self) -> String {
        self.next_internal_sub_id
            .fetch_add(1, Ordering::Relaxed)
            .to_string()
    }
}
