use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

use futures_util::future::BoxFuture;
use rig::{
    OneOrMany,
    message::{Message, Text, UserContent},
};
use smooth_protocol::{
    AgentStatus, AgentStatusChangedEvent, Event, EventMsg, Op, PlanModeChangedEvent, SessionSource,
    ThreadId, TurnCompletedEvent, TurnInterruptedEvent, TurnStartedEvent,
};
use tokio::sync::{Mutex, RwLock, broadcast};
use tools::AskUserClient;
use tracing::Instrument;

use crate::{
    agent::AgentControl,
    context_manager::ContextManager,
    error::{CoreError, CoreResult},
    provider::{SessionModel, SessionModelFactory},
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
    turn_closed: tokio::sync::Notify,
    #[allow(dead_code)]
    pub(crate) session_source: SessionSource,
    pub(crate) agent_control: AgentControl,
    current_turn_id: Arc<RwLock<Option<String>>>,
    #[allow(dead_code)]
    ask_user_client: Option<AskUserClient>,
    next_internal_sub_id: AtomicU64,
    model: Mutex<SessionModel>,
    plan_mode: AtomicBool,
    pub(crate) model_factory: Arc<dyn SessionModelFactory>,
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
        ask_user_client: Option<AskUserClient>,
        session_source: SessionSource,
        agent_control: AgentControl,
        plan_mode: bool,
        model_factory: Arc<dyn SessionModelFactory>,
    ) -> Self {
        let (event_tx, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let mut context_manager = ContextManager::default();
        context_manager.replace(history);
        let session = Arc::new(Session {
            id,
            event_tx,
            state: Mutex::new(SessionState::new(context_manager)),
            active_turn: Mutex::new(None),
            turn_closed: tokio::sync::Notify::new(),
            session_source,
            agent_control,
            current_turn_id,
            ask_user_client,
            next_internal_sub_id: AtomicU64::new(next_internal_sub_id),
            model: Mutex::new(model),
            plan_mode: AtomicBool::new(plan_mode),
            model_factory,
            rollout,
        });
        Self { session }
    }

    pub async fn start_user_input(&self, input: String) -> CoreResult<String> {
        self.submit(Op::UserInput(input)).await
    }

    pub async fn submit(&self, op: Op) -> CoreResult<String> {
        match op {
            Op::UserInput(input) => {
                let sub_id = self.session.start_regular_turn(vec![input]).await?;
                Ok(sub_id)
            }
            Op::Interrupt => {
                if self.session.abort_all_tasks("interrupted").await {
                    self.session
                        .set_agent_status(AgentStatus::Interrupted, None)
                        .await;
                    Ok("interrupted".to_string())
                } else {
                    Ok("idle".to_string())
                }
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

    pub(crate) async fn flush_rollout(&self) -> CoreResult<()> {
        self.session
            .rollout
            .flush()
            .await
            .map_err(CoreError::rollout)
    }
}

impl Session {
    fn start_regular_turn(
        self: &Arc<Self>,
        input: Vec<String>,
    ) -> BoxFuture<'_, CoreResult<String>> {
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
            .await?;
            Ok(sub_id)
        })
    }

    pub(crate) fn start_task(
        self: &Arc<Self>,
        turn_context: Arc<TurnContext>,
        input: Vec<String>,
        task: Arc<dyn AnySessionTask>,
        task_kind: crate::state::TaskKind,
    ) -> BoxFuture<'_, CoreResult<()>> {
        Box::pin(async move {
            self.wait_for_finishing_turn().await;
            self.abort_all_tasks("replaced").await;
            self.wait_for_finishing_turn().await;
            let cancellation_token = tokio_util::sync::CancellationToken::new();
            let done = Arc::new(tokio::sync::Notify::new());
            let sess = Arc::clone(self);
            let task_for_runner = Arc::clone(&task);
            let task_name = task_for_runner.span_name();
            let ctx_for_runner = Arc::clone(&turn_context);
            let done_for_runner = Arc::clone(&done);
            let cancellation_for_runner = cancellation_token.clone();
            let start_gate = Arc::new(tokio::sync::Notify::new());
            let start_gate_for_runner = Arc::clone(&start_gate);

            let task_span = tracing::info_span!(
                "core.session_task",
                thread_id = %self.id,
                turn_id = %turn_context.sub_id,
                task_name,
                task_kind = ?task_for_runner.kind(),
            );
            let handle = match tokio::task::Builder::new()
                .name(task_name)
                .spawn(
                    async move {
                        start_gate_for_runner.notified().await;
                        let result = task_for_runner
                            .run(
                                Arc::clone(&sess),
                                Arc::clone(&ctx_for_runner),
                                input,
                                cancellation_for_runner.clone(),
                            )
                            .await;

                        let (removed_task, active_turn_empty) = {
                            let mut active_turn = sess.active_turn.lock().await;
                            if let Some(turn) = active_turn.as_mut() {
                                let removed_task = turn.take_task(&ctx_for_runner.sub_id);
                                let active_turn_empty = turn.is_empty();
                                (removed_task, active_turn_empty)
                            } else {
                                (None, false)
                            }
                        };
                        let task_was_active = removed_task.is_some();

                        if cancellation_for_runner.is_cancelled() {
                            if task_was_active {
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
                            } else {
                                tracing::debug!(
                                    thread_id = %sess.id,
                                    turn_id = %ctx_for_runner.sub_id,
                                    "cancelled task ended after active turn was already drained; suppressing duplicate interruption event"
                                );
                            }
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

                        if active_turn_empty {
                            let mut active_turn = sess.active_turn.lock().await;
                            if active_turn.as_ref().is_some_and(ActiveTurn::is_empty) {
                                *active_turn = None;
                                *sess.current_turn_id.write().await = None;
                                sess.turn_closed.notify_waiters();
                            }
                        }
                        done_for_runner.notify_waiters();
                        drop(removed_task);
                    }
                    .instrument(task_span),
                ) {
                Ok(handle) => handle,
                Err(source) => {
                    let error = CoreError::TaskSpawn {
                        task_name,
                        source,
                    };
                    let info = error.to_error_info();
                    self.set_agent_status(AgentStatus::Errored(info.clone()), Some(&turn_context))
                        .await;
                    self.emit_event(
                        &turn_context,
                        EventMsg::Error(smooth_protocol::ErrorEvent { error: info }),
                    )
                    .await;
                    return Err(error);
                }
            };

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
            drop(active_turn);
            start_gate.notify_one();
            Ok(())
        })
    }

    #[tracing::instrument(name = "core.session.abort_all_tasks", skip(self), fields(thread_id = %self.id, reason))]
    pub(crate) async fn abort_all_tasks(self: &Arc<Self>, reason: &str) -> bool {
        let drained = {
            let mut active_turn = self.active_turn.lock().await;
            if active_turn.as_ref().is_some_and(ActiveTurn::is_empty) {
                None
            } else {
                active_turn.take().map(|mut turn| turn.drain_tasks())
            }
        };

        if let Some(tasks) = drained {
            let interrupted = !tasks.is_empty();
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
            self.turn_closed.notify_waiters();
            return interrupted;
        }
        false
    }

    async fn wait_for_finishing_turn(&self) {
        loop {
            let notified = self.turn_closed.notified();
            {
                let active_turn = self.active_turn.lock().await;
                if !active_turn.as_ref().is_some_and(ActiveTurn::is_empty) {
                    return;
                }
            }
            notified.await;
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
        let _ = self.agent_control.set_status(self.id, status.clone());
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

    pub(crate) async fn model(&self) -> SessionModel {
        self.model.lock().await.clone()
    }

    pub(crate) async fn set_model(&self, model: SessionModel) {
        *self.model.lock().await = model;
    }

    pub(crate) fn plan_mode(&self) -> bool {
        self.plan_mode.load(Ordering::Acquire)
    }

    pub(crate) fn set_plan_mode_flag(&self, enabled: bool) {
        self.plan_mode.store(enabled, Ordering::Release);
    }

    /// Re-build the thread's `SessionModel` with the requested plan-mode state
    /// and swap it in. Refuses mid-turn so the active stream is not torn down
    /// while a tool call is in flight. Use [`Self::apply_plan_mode_unchecked`]
    /// from within the tool loop itself (where holding `active_turn` is a
    /// pre-condition, not a conflict).
    pub(crate) async fn apply_plan_mode(self: &Arc<Self>, enabled: bool) -> CoreResult<bool> {
        if self.active_turn.lock().await.is_some() {
            return Err(CoreError::invariant(
                "cannot toggle plan mode while a turn is in flight",
            ));
        }
        self.apply_plan_mode_unchecked(enabled).await
    }

    /// Rebuild + swap without the in-turn guard. Used by the `exit_plan_mode`
    /// tool handler, which by definition runs inside the parent turn's tool
    /// loop.
    pub(crate) async fn apply_plan_mode_unchecked(
        self: &Arc<Self>,
        enabled: bool,
    ) -> CoreResult<bool> {
        if self.plan_mode() == enabled {
            return Ok(enabled);
        }
        let cwd = std::env::current_dir()?;
        let role_override = crate::agent::role::role_override_from_source(&self.session_source);
        let new_model = self
            .model_factory
            .build(
                cwd,
                self.id,
                self.ask_user_client.clone(),
                Arc::clone(&self.current_turn_id),
                role_override,
                self.agent_control.clone(),
                enabled,
            )
            .map_err(CoreError::provider)?;
        self.set_model(new_model).await;
        self.set_plan_mode_flag(enabled);
        self.emit_session_event(EventMsg::PlanModeChanged(PlanModeChangedEvent {
            thread_id: self.id.to_string(),
            enabled,
        }))
        .await;
        Ok(enabled)
    }

    pub(crate) async fn abort_pending_server_requests(&self) {
        if let Some(ask_user_client) = &self.ask_user_client {
            ask_user_client.abort_pending_server_requests(self.id).await;
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
    use std::{
        path::PathBuf,
        sync::{Arc, Mutex},
    };

    use anyhow::Result;
    use rig::message::Message;
    use tempfile::TempDir;
    use tokio::sync::RwLock;
    use tools::AskUserClient;

    use crate::{
        SessionModel, SessionModelDriver, SessionStream,
        agent::AgentControl,
        provider::{SessionModelFactory, stub_session_model_factory},
        rollout::RolloutRecorder,
        state::TaskKind,
        tasks::SessionTask,
    };

    use super::{Core, Session, TurnContext};
    use smooth_protocol::{AgentStatus, AgentStatusChangedEvent, EventMsg, SessionSource};
    use tokio_util::sync::CancellationToken;

    struct EmptyDriver;

    impl SessionModelDriver for EmptyDriver {
        fn stream_turn(&self, _prompt: Message, _history: Vec<Message>) -> Result<SessionStream> {
            Ok(Box::pin(futures_util::stream::iter(vec![Ok(
                crate::provider::SessionStreamEvent::StreamAssistantItem(
                    crate::SessionAssistantContent::Text(rig::message::Text {
                        text: "done".to_string(),
                    }),
                ),
            )])))
        }
    }

    struct CancellationAwareTask;

    impl SessionTask for CancellationAwareTask {
        fn kind(&self) -> TaskKind {
            TaskKind::Regular
        }

        fn span_name(&self) -> &'static str {
            "test.cancellation_aware_task"
        }

        async fn run(
            self: Arc<Self>,
            _session: Arc<Session>,
            _ctx: Arc<TurnContext>,
            _input: Vec<String>,
            cancellation_token: CancellationToken,
        ) -> Option<String> {
            let _ = self;
            cancellation_token.cancelled().await;
            None
        }

        async fn abort(&self, _session: Arc<Session>, _ctx: Arc<TurnContext>) {
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
        }
    }

    struct RecordingFactory {
        calls: Arc<Mutex<Vec<(bool, bool)>>>,
        model: SessionModel,
    }

    impl SessionModelFactory for RecordingFactory {
        fn build(
            &self,
            _cwd: PathBuf,
            _thread_id: smooth_protocol::ThreadId,
            ask_user_client: Option<AskUserClient>,
            _current_turn_id: Arc<RwLock<Option<String>>>,
            _role_override: crate::agent::role::RoleOverride,
            _agent_control: AgentControl,
            plan_mode: bool,
        ) -> Result<SessionModel> {
            self.calls
                .lock()
                .map_err(|_| anyhow::anyhow!("recording factory mutex should lock"))?
                .push((ask_user_client.is_some(), plan_mode));
            Ok(self.model.clone())
        }
    }

    fn stub_ask_user_client() -> AskUserClient {
        AskUserClient::new(
            |_params| async {
                Ok(app_server_protocol::AskUserQuestionResponse {
                    answers: Vec::new(),
                })
            },
            |_thread_id| async {},
        )
    }

    async fn test_core() -> Result<(
        Core,
        tokio::sync::broadcast::Receiver<smooth_protocol::Event>,
    )> {
        let workspace = TempDir::new()?;
        let cwd = PathBuf::from(workspace.path());
        let thread_id = smooth_protocol::ThreadId::new();
        let current_turn_id = Arc::new(RwLock::new(None));
        let rollout = RolloutRecorder::create(workspace.path(), thread_id, &cwd).await?;
        let model = SessionModel::Stub(Arc::new(EmptyDriver));
        let factory: Arc<dyn SessionModelFactory> =
            stub_session_model_factory(std::iter::once((thread_id, model.clone())).collect());
        let core = Core::new(
            thread_id,
            model,
            Vec::new(),
            0,
            rollout,
            current_turn_id,
            None,
            SessionSource::Cli,
            AgentControl::new(),
            false,
            factory,
        );
        let rx = core.subscribe();
        Ok((core, rx))
    }

    #[tokio::test]
    async fn submit_interrupt_without_active_turn_is_noop() -> Result<()> {
        let (core, mut rx) = test_core().await?;

        let result = core.submit(smooth_protocol::Op::Interrupt).await?;
        assert_eq!(result, "idle");
        assert!(matches!(
            rx.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
        Ok(())
    }

    #[tokio::test]
    async fn interrupt_emits_one_turn_interrupted_when_runner_observes_cancel() -> Result<()> {
        let (core, mut rx) = test_core().await?;
        let turn_context = Arc::new(core.session.fresh_turn_context());
        let turn_id = turn_context.sub_id.clone();
        core.session
            .start_task(
                turn_context,
                vec!["hello".to_string()],
                Arc::new(CancellationAwareTask),
                TaskKind::Regular,
            )
            .await?;

        let result = core.submit(smooth_protocol::Op::Interrupt).await?;
        assert_eq!(result, "interrupted");
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }

        let mut interrupted = 0;
        loop {
            match rx.try_recv() {
                Ok(event) => {
                    if matches!(
                        event.msg,
                        EventMsg::TurnInterrupted(turn) if turn.turn_id == turn_id
                    ) {
                        interrupted += 1;
                    }
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(skipped)) => {
                    panic!("test event receiver lagged by {skipped} events");
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Closed) => break,
            }
        }

        assert_eq!(interrupted, 1);
        Ok(())
    }

    #[tokio::test]
    async fn next_turn_waits_for_previous_terminal_event_after_final_status() -> Result<()> {
        let (core, mut rx) = test_core().await?;
        let first_turn_id = core.start_user_input("first".to_string()).await?;

        loop {
            let event =
                tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv()).await??;
            if matches!(
                event.msg,
                EventMsg::AgentStatusChanged(AgentStatusChangedEvent {
                    turn_id: Some(ref turn_id),
                    status: AgentStatus::Completed(_),
                    ..
                }) if turn_id == &first_turn_id
            ) {
                break;
            }
        }

        let second_turn_id = core.start_user_input("second".to_string()).await?;
        let mut saw_first_completed = false;
        loop {
            let event =
                tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv()).await??;
            match event.msg {
                EventMsg::TurnCompleted(turn) if turn.turn_id == first_turn_id => {
                    saw_first_completed = true;
                }
                EventMsg::TurnStarted(turn) if turn.turn_id == second_turn_id => {
                    assert!(
                        saw_first_completed,
                        "new turn started before previous terminal event was broadcast"
                    );
                    return Ok(());
                }
                _ => {}
            }
        }
    }

    #[tokio::test]
    async fn submit_shutdown_emits_shutdown_status() -> Result<()> {
        let (core, mut rx) = test_core().await?;

        let result = core.submit(smooth_protocol::Op::Shutdown).await?;
        assert_eq!(result, "shutdown");

        let event = rx.recv().await?;
        assert_eq!(
            event.msg,
            EventMsg::AgentStatusChanged(AgentStatusChangedEvent {
                thread_id: core.session.id.to_string(),
                turn_id: None,
                status: smooth_protocol::AgentStatus::Shutdown,
            })
        );
        Ok(())
    }

    #[tokio::test]
    async fn unchecked_plan_mode_rebuild_reuses_stored_ask_user_client() -> Result<()> {
        let workspace = TempDir::new()?;
        let cwd = PathBuf::from(workspace.path());
        let thread_id = smooth_protocol::ThreadId::new();
        let current_turn_id = Arc::new(RwLock::new(None));
        let rollout = RolloutRecorder::create(workspace.path(), thread_id, &cwd).await?;
        let model = SessionModel::Stub(Arc::new(EmptyDriver));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let factory: Arc<dyn SessionModelFactory> = Arc::new(RecordingFactory {
            calls: Arc::clone(&calls),
            model: model.clone(),
        });
        let core = Core::new(
            thread_id,
            model,
            Vec::new(),
            0,
            rollout,
            current_turn_id,
            Some(stub_ask_user_client()),
            SessionSource::Cli,
            AgentControl::new(),
            true,
            factory,
        );

        core.session.apply_plan_mode_unchecked(false).await?;

        assert_eq!(
            *calls
                .lock()
                .map_err(|_| anyhow::anyhow!("recording factory mutex should lock"))?,
            vec![(true, false)]
        );
        Ok(())
    }
}
