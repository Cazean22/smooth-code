use std::collections::BTreeMap;
use std::path::PathBuf;
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
    AgentStatus, AgentStatusChangedEvent, Event, EventMsg, Op, PlanModeChangedEvent,
    ProjectInstructions, SessionSource, ThreadId, TurnCompletedEvent, TurnInterruptedEvent,
    TurnStartedEvent,
};
use tokio::sync::{Mutex, RwLock, broadcast};
use tools::AskUserClient;
use tracing::Instrument;

use crate::{
    agent::{AgentControl, SystemPromptKind, subagent_result::CompletionEntry},
    context_manager::ContextManager,
    error::{CoreError, CoreResult},
    provider::SessionModel,
    rollout::{HistoryMessage, PersistedItem, RolloutRecorder, persisted_event_item},
    state::{ActiveTurn, RunningTask, SessionState},
    tasks::{AnySessionTask, RegularTask},
};

const EVENT_CHANNEL_CAPACITY: usize = 256;

/// Why a running turn is being cancelled. The reason picks the wire string of
/// the `TurnInterrupted` event and how long the cancelled task gets for
/// cooperative cleanup before the hard abort.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CancelReason {
    /// The user interrupted the turn (Ctrl+C / Esc / `:cancel`).
    UserInterrupt,
    /// A new turn replaces the running one.
    Replaced,
    /// The thread is shutting down.
    Shutdown,
    /// A child agent is being closed by its parent.
    Closed,
}

impl CancelReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::UserInterrupt => "interrupted",
            Self::Replaced => "replaced",
            Self::Shutdown => "shutdown",
            Self::Closed => "closed",
        }
    }

    /// How long the cancelled task gets to run cooperative cleanup (tool
    /// grace drain, partial-turn persistence) before `drain_aborted_tasks`
    /// hard-aborts it. Must exceed `tool_drain_grace()` for the same reason
    /// so the tool-batch outcome and persistence land before the hard abort;
    /// exit paths use a shorter window so shutdown stays snappy.
    pub(crate) fn drain_grace(self) -> std::time::Duration {
        match self {
            Self::UserInterrupt | Self::Replaced => std::time::Duration::from_secs(8),
            Self::Shutdown | Self::Closed => std::time::Duration::from_secs(3),
        }
    }

    /// Budget a cancelled tool batch gets to let in-flight tool futures
    /// resolve before placeholder interrupted results are synthesized. Kept
    /// strictly under `drain_grace()` so the batch always finishes —
    /// placeholders, `ToolCallCompleted` events, partial-turn persistence —
    /// before the task's hard-abort deadline. `run_command` resolves
    /// immediately on cancel (its process-group kill is detached), so even
    /// the short exit-path budget is comfortable.
    pub(crate) fn tool_drain_grace(self) -> std::time::Duration {
        match self {
            Self::UserInterrupt | Self::Replaced => std::time::Duration::from_secs(5),
            Self::Shutdown | Self::Closed => std::time::Duration::from_millis(1500),
        }
    }

    /// The terminal status published for a thread whose turn this reason
    /// cancelled.
    pub(crate) fn agent_status(self) -> AgentStatus {
        match self {
            Self::UserInterrupt | Self::Replaced => AgentStatus::Interrupted,
            Self::Shutdown | Self::Closed => AgentStatus::Shutdown,
        }
    }
}

/// Phase 2 of cancellation: wait for each cancelled task to finish its
/// cooperative cleanup, hard-aborting whatever is still running once `grace`
/// elapses. The grace is one shared window for the whole batch — all tasks
/// are drained concurrently against the same deadline, so N stubborn tasks
/// cost one grace, not N. Phase 1 ([`Session::abort_all_tasks`]) already
/// cancelled the tokens, ran the abort hooks, and emitted `TurnInterrupted`.
pub(crate) async fn drain_aborted_tasks(tasks: Vec<RunningTask>, grace: std::time::Duration) {
    if tasks.is_empty() {
        return;
    }
    let deadline = tokio::time::Instant::now() + grace;
    let drains = tasks.into_iter().map(|task| async move {
        // Register interest before checking completion so a runner that
        // notifies `done` between the check and the await is not missed.
        let notified = task.done.notified();
        if task.handle.is_finished() {
            return;
        }
        if tokio::time::timeout_at(deadline, notified).await.is_err() {
            tracing::warn!(
                turn_id = %task.turn_context.sub_id,
                grace_ms = grace.as_millis() as u64,
                "cancelled task did not finish within the drain grace; hard-aborting"
            );
            task.handle.abort();
        }
    });
    futures_util::future::join_all(drains).await;
}

/// The session's models for both plan-mode states, built once at thread
/// creation. Toggling plan mode selects between them instead of rebuilding
/// from the environment, so provider state (e.g. the parked OpenAI websocket)
/// survives toggles untouched.
pub(crate) struct SessionModels {
    normal: SessionModel,
    plan: SessionModel,
}

impl SessionModels {
    pub(crate) fn new(normal: SessionModel, plan: SessionModel) -> Self {
        Self { normal, plan }
    }

    fn for_mode(&self, plan_mode: bool) -> &SessionModel {
        if plan_mode { &self.plan } else { &self.normal }
    }
}

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
    #[allow(dead_code)]
    pub(crate) system_prompt_kind: SystemPromptKind,
    pub(crate) project_instructions: Option<ProjectInstructions>,
    pub(crate) agent_control: AgentControl,
    current_turn_id: Arc<RwLock<Option<String>>>,
    ask_user_client: Option<AskUserClient>,
    next_internal_sub_id: AtomicU64,
    models: SessionModels,
    plan_mode: AtomicBool,
    /// The most recent reason `abort_all_tasks` cancelled a turn with. The
    /// turn task only holds a token, so follow-up cancellations it performs
    /// itself — e.g. interrupting children whose spawn completed after the
    /// cascade walk — read this to carry the same reason instead of guessing.
    last_cancel_reason: std::sync::Mutex<CancelReason>,
    pub(crate) cwd: PathBuf,
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
        models: SessionModels,
        history: Vec<Message>,
        next_internal_sub_id: u64,
        rollout: RolloutRecorder,
        current_turn_id: Arc<RwLock<Option<String>>>,
        ask_user_client: Option<AskUserClient>,
        session_source: SessionSource,
        system_prompt_kind: SystemPromptKind,
        project_instructions: Option<ProjectInstructions>,
        agent_control: AgentControl,
        plan_mode: bool,
        cwd: PathBuf,
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
            system_prompt_kind,
            project_instructions,
            models,
            plan_mode: AtomicBool::new(plan_mode),
            last_cancel_reason: std::sync::Mutex::new(CancelReason::UserInterrupt),
            cwd,
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
                let interrupted = self.interrupt_turn_cascade().await;
                if interrupted.is_empty() {
                    Ok("idle".to_string())
                } else {
                    Ok("interrupted".to_string())
                }
            }
            Op::Shutdown => {
                // Own tasks and descendants' tasks drain together under one
                // shared shutdown window so N stubborn turns cost one grace,
                // not N — the TUI exit epilogue budgets a single window.
                let mut tasks = self.session.abort_all_tasks(CancelReason::Shutdown).await;
                let (_descendants, descendant_tasks) = self
                    .session
                    .agent_control
                    .abort_descendants(self.session.id, CancelReason::Shutdown)
                    .await;
                tasks.extend(descendant_tasks);
                drain_aborted_tasks(tasks, CancelReason::Shutdown.drain_grace()).await;
                self.session
                    .set_agent_status(AgentStatus::Shutdown, None)
                    .await;
                Ok("shutdown".to_string())
            }
        }
    }

    /// Interrupt this thread's running turn and cascade to every live
    /// descendant agent. Returns the thread ids (this thread included) that
    /// actually had a turn interrupted. The combined cooperative-cleanup
    /// drain runs in the background under one shared grace window —
    /// interruption must respond immediately.
    pub(crate) async fn interrupt_turn_cascade(&self) -> Vec<ThreadId> {
        let mut interrupted = Vec::new();
        let mut tasks = self
            .session
            .abort_all_tasks(CancelReason::UserInterrupt)
            .await;
        if !tasks.is_empty() {
            self.session
                .set_agent_status(AgentStatus::Interrupted, None)
                .await;
            interrupted.push(self.session.id);
        }
        let (descendants, descendant_tasks) = self
            .session
            .agent_control
            .abort_descendants(self.session.id, CancelReason::UserInterrupt)
            .await;
        interrupted.extend(descendants);
        tasks.extend(descendant_tasks);
        if !tasks.is_empty() {
            tokio::spawn(drain_aborted_tasks(
                tasks,
                CancelReason::UserInterrupt.drain_grace(),
            ));
        }
        interrupted
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.session.event_tx.subscribe()
    }

    pub(crate) async fn emit_session_event(&self, msg: EventMsg) {
        self.session.emit_session_event(msg).await;
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
            // Replacement awaits the drain inline: the old task may still be
            // writing history/rollout during its cooperative cleanup, and the
            // new turn must not interleave with it. Only an actually-replaced
            // turn cascades to descendants — its in-flight children are now
            // running against a superseded conversation; an idle session
            // starting a fresh turn must leave retained subagents from
            // completed turns alone. Own and descendant tasks drain together
            // under one shared grace window.
            let mut aborted = self.abort_all_tasks(CancelReason::Replaced).await;
            if !aborted.is_empty() {
                let (_descendants, descendant_tasks) = self
                    .agent_control
                    .abort_descendants(self.id, CancelReason::Replaced)
                    .await;
                aborted.extend(descendant_tasks);
            }
            drain_aborted_tasks(aborted, CancelReason::Replaced.drain_grace()).await;
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
                handle: tokio_util::task::AbortOnDropHandle::new(handle),
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

    #[tracing::instrument(name = "core.session.abort_all_tasks", skip(self), fields(thread_id = %self.id, reason = reason.as_str()))]
    pub(crate) async fn abort_all_tasks(
        self: &Arc<Self>,
        reason: CancelReason,
    ) -> Vec<RunningTask> {
        let drained = {
            let mut active_turn = self.active_turn.lock().await;
            if active_turn.as_ref().is_some_and(ActiveTurn::is_empty) {
                None
            } else {
                active_turn.take().map(|mut turn| turn.drain_tasks())
            }
        };

        if let Some(tasks) = drained {
            if let Ok(mut last_reason) = self.last_cancel_reason.lock() {
                *last_reason = reason;
            }
            *self.current_turn_id.write().await = None;
            for task in &tasks {
                task.cancellation_token.cancel();
                task.task
                    .abort(Arc::clone(self), Arc::clone(&task.turn_context))
                    .await;
                self.emit_event(
                    task.turn_context.as_ref(),
                    EventMsg::TurnInterrupted(TurnInterruptedEvent {
                        thread_id: self.id.to_string(),
                        turn_id: task.turn_context.sub_id.clone(),
                        reason: reason.as_str().to_string(),
                    }),
                )
                .await;
            }
            self.turn_closed.notify_waiters();
            return tasks;
        }
        Vec::new()
    }

    /// Cancel the running turn and wait — bounded by the reason's grace — for
    /// the task's cooperative cleanup to finish, hard-aborting at the
    /// deadline. Returns whether anything was interrupted.
    pub(crate) async fn abort_and_drain_all_tasks(self: &Arc<Self>, reason: CancelReason) -> bool {
        let tasks = self.abort_all_tasks(reason).await;
        let interrupted = !tasks.is_empty();
        drain_aborted_tasks(tasks, reason.drain_grace()).await;
        interrupted
    }

    /// The reason of the most recent `abort_all_tasks` that drained a task;
    /// defaults to `UserInterrupt` before any cancellation. Read by the turn
    /// task, which only holds a token, when it must cancel something itself
    /// after observing that token.
    pub(crate) fn last_cancel_reason(&self) -> CancelReason {
        self.last_cancel_reason
            .lock()
            .map(|reason| *reason)
            .unwrap_or(CancelReason::UserInterrupt)
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
        let message = Message::User {
            content: OneOrMany::one(UserContent::Text(Text { text })),
        };
        let mut state = self.state.lock().await;
        state.history.push(message.clone());
        drop(state);
        let _ = self
            .rollout
            .append(PersistedItem::HistoryMessage(HistoryMessage::Full {
                message,
            }))
            .await;
    }

    /// Persist the turn's model-facing tail (everything after the
    /// already-recorded prompt at index 0) to the rollout. Tail indices present
    /// in `completions_by_index` are written as typed `SubagentCompletion`
    /// records — the durable source of truth — instead of the rendered
    /// `Message::User`; every other tail message is written as `Full`. Returns
    /// `false` if any append failed, so callers can skip irreversible follow-up
    /// (e.g. closing consumed-child edges) when the result is not durable.
    pub(crate) async fn persist_turn_tail(
        &self,
        new_messages: &[Message],
        completions_by_index: &BTreeMap<usize, Vec<CompletionEntry>>,
    ) -> bool {
        let mut persisted = true;
        for (index, message) in new_messages.iter().enumerate().skip(1) {
            let item = match completions_by_index.get(&index) {
                Some(completions) => {
                    PersistedItem::HistoryMessage(HistoryMessage::SubagentCompletion {
                        completions: completions.clone(),
                    })
                }
                None => PersistedItem::HistoryMessage(HistoryMessage::Full {
                    message: message.clone(),
                }),
            };
            if let Err(err) = self.rollout.append(item).await {
                tracing::warn!(
                    thread_id = %self.id,
                    error = %err,
                    "failed to persist turn-tail item to rollout"
                );
                persisted = false;
            }
        }
        persisted
    }

    pub(crate) async fn emit_event(&self, ctx: &TurnContext, msg: EventMsg) {
        if let Some(item) = persisted_event_item(&msg) {
            let _ = self.rollout.append(item).await;
        }
        let _ = self.event_tx.send(Event {
            id: ctx.sub_id.clone(),
            msg,
        });
    }

    pub(crate) async fn emit_session_event(&self, msg: EventMsg) {
        if let Some(item) = persisted_event_item(&msg) {
            let _ = self.rollout.append(item).await;
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

    pub(crate) fn model(&self) -> SessionModel {
        self.models.for_mode(self.plan_mode()).clone()
    }

    pub(crate) fn ask_user_client(&self) -> Option<&AskUserClient> {
        self.ask_user_client.as_ref()
    }

    pub(crate) fn plan_mode(&self) -> bool {
        self.plan_mode.load(Ordering::Acquire)
    }

    pub(crate) fn set_plan_mode_flag(&self, enabled: bool) {
        self.plan_mode.store(enabled, Ordering::Release);
    }

    /// Flip the thread's plan-mode state. Refuses mid-turn so the active
    /// stream's tool set does not change underneath a tool call already in
    /// flight. Use [`Self::apply_plan_mode_unchecked`] from within the tool
    /// loop itself (where holding `active_turn` is a pre-condition, not a
    /// conflict).
    pub(crate) async fn apply_plan_mode(self: &Arc<Self>, enabled: bool) -> CoreResult<bool> {
        // Hold the guard across the flip so a turn cannot start between the
        // check and the state change.
        let active_turn = self.active_turn.lock().await;
        if active_turn.is_some() {
            return Err(CoreError::invariant(
                "cannot toggle plan mode while a turn is in flight",
            ));
        }
        self.apply_plan_mode_unchecked(enabled).await
    }

    /// Flip plan mode without the in-turn guard. Used by the `exit_plan_mode`
    /// tool handler, which by definition runs inside the parent turn's tool
    /// loop. Both models are prebuilt at session creation, so this is just a
    /// flag store plus the `PlanModeChanged` event.
    pub(crate) async fn apply_plan_mode_unchecked(
        self: &Arc<Self>,
        enabled: bool,
    ) -> CoreResult<bool> {
        if self.plan_mode() == enabled {
            return Ok(enabled);
        }
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
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use anyhow::Result;
    use rig::{
        completion::CompletionError,
        message::{AssistantContent, Message, Text, ToolCall, ToolFunction, UserContent},
    };
    use tempfile::TempDir;
    use tokio::sync::RwLock;

    use crate::{
        SessionCompletionEvent, SessionCompletionStream, SessionModel, SessionModelDriver,
        SessionTurnSummary, agent::AgentControl, rollout::RolloutRecorder, state::TaskKind,
        tasks::SessionTask,
    };

    use super::{CancelReason, Core, Session, SessionModels, TurnContext};
    use smooth_protocol::{AgentStatus, AgentStatusChangedEvent, EventMsg, SessionSource};
    use tokio_util::sync::CancellationToken;

    struct EmptyDriver;

    impl SessionModelDriver for EmptyDriver {
        fn stream_completion_turn(
            &self,
            _prompt: Message,
            _history: Vec<Message>,
        ) -> Result<SessionCompletionStream> {
            Ok(Box::pin(futures_util::stream::iter(vec![
                Ok(SessionCompletionEvent::AssistantItem(
                    crate::SessionAssistantContent::Text(rig::message::Text {
                        text: "done".to_string(),
                    }),
                )),
                Ok(SessionCompletionEvent::Completed(SessionTurnSummary {
                    assistant_message_id: Some("assistant-done".to_string()),
                    response: "done".to_string(),
                })),
            ])))
        }
    }

    struct ResetOnceDriver {
        calls: AtomicUsize,
    }

    impl ResetOnceDriver {
        fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
            }
        }
    }

    impl SessionModelDriver for ResetOnceDriver {
        fn stream_completion_turn(
            &self,
            _prompt: Message,
            _history: Vec<Message>,
        ) -> Result<SessionCompletionStream> {
            let call_index = self.calls.fetch_add(1, Ordering::SeqCst);
            if call_index == 0 {
                let events: Vec<Result<SessionCompletionEvent>> = vec![
                    Ok(SessionCompletionEvent::AssistantItem(
                        crate::SessionAssistantContent::Text(Text {
                            text: "partial ".to_string(),
                        }),
                    )),
                    Err(anyhow::Error::new(CompletionError::ProviderError(
                        "OpenAI WebSocket connection reset before response.completed".to_string(),
                    ))),
                ];
                return Ok(Box::pin(futures_util::stream::iter(events)));
            }

            Ok(Box::pin(futures_util::stream::iter(vec![
                Ok(SessionCompletionEvent::AssistantItem(
                    crate::SessionAssistantContent::Text(Text {
                        text: "continued".to_string(),
                    }),
                )),
                Ok(SessionCompletionEvent::Completed(SessionTurnSummary {
                    assistant_message_id: Some("assistant-continued".to_string()),
                    response: "continued".to_string(),
                })),
            ])))
        }
    }

    struct ResetAfterToolDriver {
        calls: AtomicUsize,
        tool_calls: AtomicUsize,
    }

    impl ResetAfterToolDriver {
        fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
                tool_calls: AtomicUsize::new(0),
            }
        }
    }

    impl SessionModelDriver for ResetAfterToolDriver {
        fn stream_completion_turn(
            &self,
            _prompt: Message,
            _history: Vec<Message>,
        ) -> Result<SessionCompletionStream> {
            let call_index = self.calls.fetch_add(1, Ordering::SeqCst);
            if call_index == 0 {
                let tool_call = ToolCall::new(
                    "tool-1".to_string(),
                    ToolFunction::new("test_tool".to_string(), serde_json::json!({ "x": 1 })),
                )
                .with_call_id("call-1".to_string());
                let events: Vec<Result<SessionCompletionEvent>> = vec![
                    Ok(SessionCompletionEvent::AssistantItem(
                        crate::SessionAssistantContent::ToolCall {
                            tool_call,
                            internal_call_id: "internal-tool-1".to_string(),
                        },
                    )),
                    Err(anyhow::Error::new(CompletionError::ProviderError(
                        "OpenAI WebSocket connection reset before response.completed".to_string(),
                    ))),
                ];
                return Ok(Box::pin(futures_util::stream::iter(events)));
            }

            Ok(Box::pin(futures_util::stream::iter(vec![
                Ok(SessionCompletionEvent::AssistantItem(
                    crate::SessionAssistantContent::Text(Text {
                        text: "after tool".to_string(),
                    }),
                )),
                Ok(SessionCompletionEvent::Completed(SessionTurnSummary {
                    assistant_message_id: Some("assistant-after-tool".to_string()),
                    response: "after tool".to_string(),
                })),
            ])))
        }

        fn call_tool(
            &self,
            tool_name: &str,
            args: &str,
        ) -> futures_util::future::BoxFuture<'static, Result<String>> {
            assert_eq!(tool_name, "test_tool");
            assert_eq!(args, r#"{"x":1}"#);
            self.tool_calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok("tool-output".to_string()) })
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

    /// A task that ignores its cancellation token entirely — the worst case
    /// the drain's hard-abort deadline exists for.
    struct StubbornTask;

    impl SessionTask for StubbornTask {
        fn kind(&self) -> TaskKind {
            TaskKind::Regular
        }

        fn span_name(&self) -> &'static str {
            "test.stubborn_task"
        }

        async fn run(
            self: Arc<Self>,
            _session: Arc<Session>,
            _ctx: Arc<TurnContext>,
            _input: Vec<String>,
            _cancellation_token: CancellationToken,
        ) -> Option<String> {
            let _ = self;
            std::future::pending().await
        }
    }

    /// Streams one tool call on the first attempt, then completes. The tool
    /// either cooperates with cancellation (resolving with an interrupted
    /// error once the ambient token cancels) or hangs forever, exercising the
    /// synthesized-placeholder path.
    struct InterruptibleToolDriver {
        calls: AtomicUsize,
        hang_forever: bool,
    }

    impl InterruptibleToolDriver {
        fn new(hang_forever: bool) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                hang_forever,
            }
        }
    }

    impl SessionModelDriver for InterruptibleToolDriver {
        fn stream_completion_turn(
            &self,
            _prompt: Message,
            _history: Vec<Message>,
        ) -> Result<SessionCompletionStream> {
            let call_index = self.calls.fetch_add(1, Ordering::SeqCst);
            if call_index == 0 {
                let tool_call = ToolCall::new(
                    "tool-1".to_string(),
                    ToolFunction::new("slow_tool".to_string(), serde_json::json!({})),
                )
                .with_call_id("call-1".to_string());
                return Ok(Box::pin(futures_util::stream::iter(vec![Ok(
                    SessionCompletionEvent::AssistantItem(
                        crate::SessionAssistantContent::ToolCall {
                            tool_call,
                            internal_call_id: "internal-tool-1".to_string(),
                        },
                    ),
                )])));
            }
            Ok(Box::pin(futures_util::stream::iter(vec![Ok(
                SessionCompletionEvent::Completed(SessionTurnSummary {
                    assistant_message_id: Some("assistant-after-tool".to_string()),
                    response: "after tool".to_string(),
                }),
            )])))
        }

        fn call_tool(
            &self,
            _tool_name: &str,
            _args: &str,
        ) -> futures_util::future::BoxFuture<'static, Result<String>> {
            if self.hang_forever {
                Box::pin(std::future::pending())
            } else {
                Box::pin(async {
                    tools::tool_cancel_token().cancelled().await;
                    Err(anyhow::Error::new(tools::ToolError::interrupted(
                        "tool interrupted by cancellation",
                    )))
                })
            }
        }
    }

    async fn test_core_with_driver(
        driver: Arc<dyn SessionModelDriver>,
    ) -> Result<(
        Core,
        tokio::sync::broadcast::Receiver<smooth_protocol::Event>,
        TempDir,
    )> {
        let workspace = TempDir::new()?;
        let cwd = PathBuf::from(workspace.path());
        let thread_id = smooth_protocol::ThreadId::new();
        let current_turn_id = Arc::new(RwLock::new(None));
        let rollout = RolloutRecorder::create(workspace.path(), thread_id, &cwd).await?;
        let model = SessionModel::Stub(driver);
        let core = Core::new(
            thread_id,
            SessionModels::new(model.clone(), model),
            Vec::new(),
            0,
            rollout,
            current_turn_id,
            None,
            SessionSource::Cli,
            crate::agent::SystemPromptKind::Root,
            None,
            AgentControl::new(),
            false,
            cwd,
        );
        let rx = core.subscribe();
        Ok((core, rx, workspace))
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
        let core = Core::new(
            thread_id,
            SessionModels::new(model.clone(), model),
            Vec::new(),
            0,
            rollout,
            current_turn_id,
            None,
            SessionSource::Cli,
            crate::agent::SystemPromptKind::Root,
            None,
            AgentControl::new(),
            false,
            cwd,
        );
        let rx = core.subscribe();
        Ok((core, rx))
    }

    fn user_message_text(message: &Message) -> Option<String> {
        match message {
            Message::User { content } => Some(
                content
                    .iter()
                    .filter_map(|content| match content {
                        UserContent::Text(text) => Some(text.text.as_str()),
                        _ => None,
                    })
                    .collect::<String>(),
            ),
            Message::Assistant { .. } | Message::System { .. } => None,
        }
    }

    fn assistant_message_text(message: &Message) -> Option<String> {
        match message {
            Message::Assistant { content, .. } => Some(
                content
                    .iter()
                    .filter_map(|content| match content {
                        AssistantContent::Text(text) => Some(text.text.as_str()),
                        _ => None,
                    })
                    .collect::<String>(),
            ),
            Message::User { .. } | Message::System { .. } => None,
        }
    }

    fn assistant_tool_call_count(message: &Message) -> Option<usize> {
        match message {
            Message::Assistant { content, .. } => Some(
                content
                    .iter()
                    .filter(|content| matches!(content, AssistantContent::ToolCall(_)))
                    .count(),
            ),
            Message::User { .. } | Message::System { .. } => None,
        }
    }

    fn user_tool_result_count(message: &Message) -> Option<usize> {
        match message {
            Message::User { content } => Some(
                content
                    .iter()
                    .filter(|content| matches!(content, UserContent::ToolResult(_)))
                    .count(),
            ),
            Message::Assistant { .. } | Message::System { .. } => None,
        }
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

    /// Wait until the session history reaches `min_len`, bounded by a timeout.
    /// The interrupted turn's persistence runs in the cancelled task's
    /// cooperative-cleanup window, after the interrupt response returns.
    async fn wait_for_history_len(core: &Core, min_len: usize) -> Vec<Message> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let history = core.session.history().await;
            if history.len() >= min_len {
                return history;
            }
            if std::time::Instant::now() > deadline {
                panic!(
                    "history never reached {min_len} items; got {}",
                    history.len()
                );
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn interrupt_mid_tool_records_interrupted_result_and_persists_partial_turn() -> Result<()>
    {
        let (core, mut rx, _workspace) =
            test_core_with_driver(Arc::new(InterruptibleToolDriver::new(false))).await?;
        let turn_id = core.start_user_input("hello".to_string()).await?;

        // Wait for the tool call to start before interrupting.
        loop {
            let event =
                tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv()).await??;
            if matches!(
                event.msg,
                EventMsg::ToolCallStarted(ref started) if started.turn_id == turn_id
            ) {
                break;
            }
        }

        let result = core.submit(smooth_protocol::Op::Interrupt).await?;
        assert_eq!(result, "interrupted");

        // The cooperative tool resolves with an interrupted error once the
        // token cancels; its completion event surfaces during the drain.
        let mut saw_interrupted_completion = false;
        let mut interrupted_events = 0;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !saw_interrupted_completion && std::time::Instant::now() < deadline {
            let Ok(Ok(event)) =
                tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await
            else {
                continue;
            };
            match event.msg {
                EventMsg::ToolCallCompleted(completed) if completed.turn_id == turn_id => {
                    assert!(!completed.success);
                    assert_eq!(
                        completed.result_kind,
                        smooth_protocol::ToolCallResultKind::Interrupted
                    );
                    saw_interrupted_completion = true;
                }
                EventMsg::TurnInterrupted(turn) if turn.turn_id == turn_id => {
                    interrupted_events += 1;
                }
                _ => {}
            }
        }
        assert!(
            saw_interrupted_completion,
            "expected an interrupted ToolCallCompleted event"
        );
        assert_eq!(interrupted_events, 1);

        // The partial turn — assistant tool-call message plus the interrupted
        // tool result — is folded into history so the model sees what was cut
        // short on the next turn.
        let history = wait_for_history_len(&core, 3).await;
        assert_eq!(user_message_text(&history[0]).as_deref(), Some("hello"));
        assert_eq!(assistant_tool_call_count(&history[1]), Some(1));
        assert_eq!(user_tool_result_count(&history[2]), Some(1));
        Ok(())
    }

    #[tokio::test]
    async fn interrupt_with_hanging_tool_synthesizes_interrupted_result_after_grace() -> Result<()>
    {
        // Shrink the tool-batch grace so the synthesized-placeholder path runs
        // quickly. Process-global, but the only other consumers resolve their
        // tools cooperatively and never reach the deadline.
        unsafe { std::env::set_var("SMOOTH_CODE_TOOL_CANCEL_GRACE_MS", "100") };
        let (core, mut rx, _workspace) =
            test_core_with_driver(Arc::new(InterruptibleToolDriver::new(true))).await?;
        let turn_id = core.start_user_input("hello".to_string()).await?;

        loop {
            let event =
                tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv()).await??;
            if matches!(
                event.msg,
                EventMsg::ToolCallStarted(ref started) if started.turn_id == turn_id
            ) {
                break;
            }
        }

        let result = core.submit(smooth_protocol::Op::Interrupt).await?;
        assert_eq!(result, "interrupted");

        // The tool never resolves; after the (shrunk) grace the batch
        // completes it with a synthesized placeholder.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut saw_placeholder = false;
        while !saw_placeholder && std::time::Instant::now() < deadline {
            let Ok(Ok(event)) =
                tokio::time::timeout(std::time::Duration::from_millis(200), rx.recv()).await
            else {
                continue;
            };
            if let EventMsg::ToolCallCompleted(completed) = event.msg
                && completed.turn_id == turn_id
            {
                assert!(!completed.success);
                assert_eq!(
                    completed.result_kind,
                    smooth_protocol::ToolCallResultKind::Interrupted
                );
                assert!(
                    completed
                        .output_preview
                        .as_deref()
                        .is_some_and(|preview| preview.contains("interrupted")),
                    "unexpected preview: {:?}",
                    completed.output_preview
                );
                saw_placeholder = true;
            }
        }
        assert!(
            saw_placeholder,
            "expected a synthesized interrupted ToolCallCompleted event"
        );

        let history = wait_for_history_len(&core, 3).await;
        assert_eq!(assistant_tool_call_count(&history[1]), Some(1));
        assert_eq!(user_tool_result_count(&history[2]), Some(1));
        Ok(())
    }

    #[tokio::test]
    async fn stubborn_task_is_hard_aborted_after_grace_and_session_recovers() -> Result<()> {
        let (core, mut rx) = test_core().await?;
        let turn_context = Arc::new(core.session.fresh_turn_context());
        core.session
            .start_task(
                turn_context,
                vec!["hello".to_string()],
                Arc::new(StubbornTask),
                TaskKind::Regular,
            )
            .await?;

        let tasks = core
            .session
            .abort_all_tasks(CancelReason::UserInterrupt)
            .await;
        assert_eq!(tasks.len(), 1);
        // A tiny grace: the task ignores its token, so the drain must
        // hard-abort it at the deadline instead of waiting forever.
        super::drain_aborted_tasks(tasks, std::time::Duration::from_millis(50)).await;

        // The session accepts and completes a follow-up turn.
        let next_turn_id = core.start_user_input("again".to_string()).await?;
        loop {
            let event =
                tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv()).await??;
            if matches!(
                event.msg,
                EventMsg::TurnCompleted(ref turn) if turn.turn_id == next_turn_id
            ) {
                return Ok(());
            }
        }
    }

    #[tokio::test]
    async fn drain_grace_is_one_shared_window_for_the_whole_batch() -> Result<()> {
        // Two sessions, each with a task that ignores its token: the batch
        // must drain in ~one grace window, not one per stubborn task.
        let mut tasks = Vec::new();
        let mut cores = Vec::new();
        for _ in 0..2 {
            let (core, _rx) = test_core().await?;
            let turn_context = Arc::new(core.session.fresh_turn_context());
            core.session
                .start_task(
                    turn_context,
                    vec!["hello".to_string()],
                    Arc::new(StubbornTask),
                    TaskKind::Regular,
                )
                .await?;
            tasks.extend(
                core.session
                    .abort_all_tasks(CancelReason::UserInterrupt)
                    .await,
            );
            cores.push(core);
        }
        assert_eq!(tasks.len(), 2);

        let started = std::time::Instant::now();
        super::drain_aborted_tasks(tasks, std::time::Duration::from_millis(500)).await;
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_millis(900),
            "drain took {elapsed:?}; per-task graces would take ~1s+"
        );
        Ok(())
    }

    #[tokio::test]
    async fn abort_records_last_cancel_reason() -> Result<()> {
        let (core, _rx) = test_core().await?;
        assert_eq!(
            core.session.last_cancel_reason(),
            CancelReason::UserInterrupt
        );

        let turn_context = Arc::new(core.session.fresh_turn_context());
        core.session
            .start_task(
                turn_context,
                vec!["hello".to_string()],
                Arc::new(CancellationAwareTask),
                TaskKind::Regular,
            )
            .await?;
        core.session
            .abort_and_drain_all_tasks(CancelReason::Shutdown)
            .await;

        assert_eq!(core.session.last_cancel_reason(), CancelReason::Shutdown);
        Ok(())
    }

    /// Shutdown awaits its drain inline, and the tool-batch grace for
    /// `Shutdown` is kept under the drain grace — so by the time the
    /// shutdown response returns, the hanging tool has a placeholder
    /// interrupted result and the partial turn is in history, with no hard
    /// abort cutting cleanup short.
    #[tokio::test]
    async fn shutdown_mid_hanging_tool_persists_partial_turn_before_hard_abort() -> Result<()> {
        let (core, mut rx, _workspace) =
            test_core_with_driver(Arc::new(InterruptibleToolDriver::new(true))).await?;
        let turn_id = core.start_user_input("hello".to_string()).await?;

        loop {
            let event =
                tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv()).await??;
            if matches!(
                event.msg,
                EventMsg::ToolCallStarted(ref started) if started.turn_id == turn_id
            ) {
                break;
            }
        }

        let result = core.submit(smooth_protocol::Op::Shutdown).await?;
        assert_eq!(result, "shutdown");

        // The inline drain returned, so the cancelled task already finished
        // its cooperative cleanup: placeholder result emitted, partial turn
        // persisted.
        let mut saw_placeholder = false;
        while let Ok(event) = rx.try_recv() {
            if let EventMsg::ToolCallCompleted(completed) = event.msg
                && completed.turn_id == turn_id
            {
                assert_eq!(
                    completed.result_kind,
                    smooth_protocol::ToolCallResultKind::Interrupted
                );
                saw_placeholder = true;
            }
        }
        assert!(
            saw_placeholder,
            "expected the hanging tool's interrupted placeholder before shutdown returned"
        );

        let history = core.session.history().await;
        assert!(history.len() >= 3, "partial turn should be in history");
        assert_eq!(assistant_tool_call_count(&history[1]), Some(1));
        assert_eq!(user_tool_result_count(&history[2]), Some(1));
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
    async fn retryable_mid_reply_reset_commits_partial_and_continues() -> Result<()> {
        let workspace = TempDir::new()?;
        let cwd = PathBuf::from(workspace.path());
        let thread_id = smooth_protocol::ThreadId::new();
        let current_turn_id = Arc::new(RwLock::new(None));
        let rollout = RolloutRecorder::create(workspace.path(), thread_id, &cwd).await?;
        let driver = Arc::new(ResetOnceDriver::new());
        let model = SessionModel::Stub(driver.clone());
        let core = Core::new(
            thread_id,
            SessionModels::new(model.clone(), model),
            Vec::new(),
            0,
            rollout,
            current_turn_id,
            None,
            SessionSource::Cli,
            crate::agent::SystemPromptKind::Root,
            None,
            AgentControl::new(),
            false,
            cwd,
        );
        let mut rx = core.subscribe();

        let turn_id = core.start_user_input("hello".to_string()).await?;
        let mut events = Vec::new();
        loop {
            let event =
                tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv()).await??;
            let turn_completed = matches!(
                &event.msg,
                EventMsg::TurnCompleted(turn) if turn.turn_id == turn_id
            );
            events.push(event.msg);
            if turn_completed {
                break;
            }
        }

        assert_eq!(driver.calls.load(Ordering::SeqCst), 2);
        assert!(events.iter().any(|msg| {
            matches!(
                msg,
                EventMsg::StreamError(event)
                    if event.turn_id == turn_id && event.message == "Reconnecting… 1/8"
            )
        }));
        assert!(events.iter().any(|msg| {
            matches!(
                msg,
                EventMsg::AgentMessageCompleted(event)
                    if event.turn_id == turn_id && event.text == "partial "
            )
        }));
        assert!(events.iter().any(|msg| {
            matches!(
                msg,
                EventMsg::AgentMessageCompleted(event)
                    if event.turn_id == turn_id && event.text == "continued"
            )
        }));

        let history = core.session.history().await;
        assert_eq!(history.len(), 3);
        assert_eq!(user_message_text(&history[0]).as_deref(), Some("hello"));
        assert_eq!(
            assistant_message_text(&history[1]).as_deref(),
            Some("partial ")
        );
        assert_eq!(
            assistant_message_text(&history[2]).as_deref(),
            Some("continued")
        );
        Ok(())
    }

    #[tokio::test]
    async fn retryable_reset_after_tool_call_executes_tool_once_before_continuing() -> Result<()> {
        let workspace = TempDir::new()?;
        let cwd = PathBuf::from(workspace.path());
        let thread_id = smooth_protocol::ThreadId::new();
        let current_turn_id = Arc::new(RwLock::new(None));
        let rollout = RolloutRecorder::create(workspace.path(), thread_id, &cwd).await?;
        let driver = Arc::new(ResetAfterToolDriver::new());
        let model = SessionModel::Stub(driver.clone());
        let core = Core::new(
            thread_id,
            SessionModels::new(model.clone(), model),
            Vec::new(),
            0,
            rollout,
            current_turn_id,
            None,
            SessionSource::Cli,
            crate::agent::SystemPromptKind::Root,
            None,
            AgentControl::new(),
            false,
            cwd,
        );
        let mut rx = core.subscribe();

        let turn_id = core.start_user_input("hello".to_string()).await?;
        let mut events = Vec::new();
        loop {
            let event =
                tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv()).await??;
            let turn_completed = matches!(
                &event.msg,
                EventMsg::TurnCompleted(turn) if turn.turn_id == turn_id
            );
            events.push(event.msg);
            if turn_completed {
                break;
            }
        }

        assert_eq!(driver.calls.load(Ordering::SeqCst), 2);
        assert_eq!(driver.tool_calls.load(Ordering::SeqCst), 1);
        assert!(events.iter().any(|msg| {
            matches!(
                msg,
                EventMsg::ToolCallCompleted(event)
                    if event.turn_id == turn_id
                        && event.call_id == "internal-tool-1"
                        && event.success
                        && event.output_preview.as_deref() == Some("tool-output")
            )
        }));
        assert!(events.iter().any(|msg| {
            matches!(
                msg,
                EventMsg::StreamError(event)
                    if event.turn_id == turn_id && event.message == "Reconnecting… 1/8"
            )
        }));

        let history = core.session.history().await;
        assert_eq!(history.len(), 4);
        assert_eq!(user_message_text(&history[0]).as_deref(), Some("hello"));
        assert_eq!(assistant_tool_call_count(&history[1]), Some(1));
        assert_eq!(user_tool_result_count(&history[2]), Some(1));
        assert_eq!(
            assistant_message_text(&history[3]).as_deref(),
            Some("after tool")
        );
        Ok(())
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
    async fn toggle_plan_mode_selects_prebuilt_model_and_emits_event() -> Result<()> {
        let workspace = TempDir::new()?;
        let cwd = PathBuf::from(workspace.path());
        let thread_id = smooth_protocol::ThreadId::new();
        let current_turn_id = Arc::new(RwLock::new(None));
        let rollout = RolloutRecorder::create(workspace.path(), thread_id, &cwd).await?;
        let normal_driver: Arc<dyn SessionModelDriver> = Arc::new(EmptyDriver);
        let plan_driver: Arc<dyn SessionModelDriver> = Arc::new(EmptyDriver);
        let core = Core::new(
            thread_id,
            SessionModels::new(
                SessionModel::Stub(Arc::clone(&normal_driver)),
                SessionModel::Stub(Arc::clone(&plan_driver)),
            ),
            Vec::new(),
            0,
            rollout,
            current_turn_id,
            None,
            SessionSource::Cli,
            crate::agent::SystemPromptKind::Root,
            None,
            AgentControl::new(),
            false,
            cwd,
        );
        let mut rx = core.subscribe();

        let driver_of = |model: SessionModel| match model {
            SessionModel::Stub(driver) => Ok(driver),
            _ => Err(anyhow::anyhow!("expected stub session model")),
        };
        assert!(Arc::ptr_eq(
            &driver_of(core.session.model())?,
            &normal_driver
        ));

        let enabled = core.session.apply_plan_mode(true).await?;
        assert!(enabled);
        assert!(core.session.plan_mode());
        assert!(Arc::ptr_eq(&driver_of(core.session.model())?, &plan_driver));
        let event = rx.recv().await?;
        assert!(matches!(
            event.msg,
            EventMsg::PlanModeChanged(ref change) if change.enabled
        ));

        // Toggling to the same state is a no-op and emits nothing further.
        let enabled = core.session.apply_plan_mode(true).await?;
        assert!(enabled);

        let enabled = core.session.apply_plan_mode(false).await?;
        assert!(!enabled);
        assert!(Arc::ptr_eq(
            &driver_of(core.session.model())?,
            &normal_driver
        ));
        let event = rx.recv().await?;
        assert!(matches!(
            event.msg,
            EventMsg::PlanModeChanged(ref change) if !change.enabled
        ));
        Ok(())
    }

    #[tokio::test]
    async fn apply_plan_mode_refuses_while_turn_is_in_flight() -> Result<()> {
        let (core, _rx) = test_core().await?;

        let turn_context = Arc::new(core.session.fresh_turn_context());
        core.session
            .start_task(
                turn_context,
                Vec::new(),
                Arc::new(CancellationAwareTask),
                TaskKind::Regular,
            )
            .await?;

        let result = core.session.apply_plan_mode(true).await;
        assert!(result.is_err());
        assert!(!core.session.plan_mode());

        core.session
            .abort_and_drain_all_tasks(CancelReason::Shutdown)
            .await;
        Ok(())
    }
}
