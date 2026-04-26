use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use anyhow::{Result, anyhow};
use rig::{
    OneOrMany,
    message::{Message, Text, UserContent},
};
use smooth_protocol::{AgentStatus, Event, EventMsg, ThreadId};
use tokio::sync::{Mutex, watch};

use crate::{
    provider::SessionModel,
    state::{ActiveTurn, RunningTask, SessionState},
    tasks::{RegularTask, SessionTask},
};

pub struct Core {
    pub(crate) session: Arc<Session>,
}

/// A session has at most 1 running task at a time.
pub(crate) struct Session {
    pub(crate) id: ThreadId,
    agent_status: watch::Sender<AgentStatus>,
    state: Mutex<SessionState>,
    pub(crate) active_turn: Mutex<Option<ActiveTurn>>,
    next_internal_sub_id: AtomicU64,
    model: SessionModel,
}

/// The context needed for a single turn of the thread.
#[derive(Debug)]
pub(crate) struct TurnContext {
    pub(crate) sub_id: String,
    pub(crate) timezone: Option<String>,
}

impl Core {
    pub(crate) fn new(id: ThreadId, model: SessionModel) -> Self {
        let (agent_status, _) = watch::channel(AgentStatus::PendingInit);
        let session = Arc::new(Session {
            id,
            agent_status,
            state: Mutex::new(SessionState::new()),
            active_turn: Mutex::new(None),
            next_internal_sub_id: AtomicU64::new(0),
            model,
        });
        Self { session }
    }

    pub async fn run_user_input(&self, input: String) -> Result<String> {
        let turn_context = Arc::new(TurnContext {
            sub_id: self.session.next_internal_sub_id(),
            timezone: None,
        });

        self.session
            .run_task(turn_context, vec![input], RegularTask::new())
            .await?
            .ok_or_else(|| anyhow!("regular task finished without an assistant message"))
    }
}

impl Session {
    pub(crate) async fn run_task<T: SessionTask>(
        self: &Arc<Self>,
        turn_context: Arc<TurnContext>,
        input: Vec<String>,
        task: T,
    ) -> Result<Option<String>> {
        self.abort_all_tasks().await;

        let task: Arc<dyn crate::tasks::AnySessionTask> = Arc::new(task);
        let cancellation_token = tokio_util::sync::CancellationToken::new();
        let done = Arc::new(tokio::sync::Notify::new());
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        let sess = Arc::clone(self);
        let task_for_runner = Arc::clone(&task);
        let ctx_for_runner = Arc::clone(&turn_context);
        let done_for_runner = Arc::clone(&done);
        let cancellation_for_runner = cancellation_token.clone();

        let handle = tokio::spawn(async move {
            let result = task_for_runner
                .run(
                    Arc::clone(&sess),
                    Arc::clone(&ctx_for_runner),
                    input,
                    cancellation_for_runner,
                )
                .await;
            sess.set_agent_status(AgentStatus::Completed(result.clone()))
                .await;
            let _ = result_tx.send(result);
            done_for_runner.notify_waiters();
        });

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
        drop(active_turn);

        let result = result_rx
            .await
            .map_err(|_| anyhow!("task runner dropped"))?;

        let mut active_turn = self.active_turn.lock().await;
        if let Some(turn) = active_turn.as_mut()
            && turn.remove_task(&turn_context.sub_id)
        {
            *active_turn = None;
        }
        drop(active_turn);

        Ok(result)
    }

    pub(crate) async fn abort_all_tasks(self: &Arc<Self>) {
        let drained = {
            let mut active_turn = self.active_turn.lock().await;
            active_turn.take().map(|mut turn| turn.drain_tasks())
        };

        if let Some(tasks) = drained {
            for task in tasks {
                task.cancellation_token.cancel();
                task.task
                    .abort(Arc::clone(self), Arc::clone(&task.turn_context))
                    .await;
            }
        }
    }

    pub(crate) async fn history(&self) -> Vec<Message> {
        let state = self.state.lock().await;
        state.history.items().to_vec()
    }

    pub(crate) async fn record_user_message(&self, text: String) {
        let mut state = self.state.lock().await;
        state.history.push(Message::User {
            content: OneOrMany::one(UserContent::Text(Text { text })),
        });
    }

    pub(crate) async fn record_assistant_message(&self, text: String) {
        let mut state = self.state.lock().await;
        state.history.push(Message::Assistant {
            id: None,
            content: OneOrMany::one(rig::message::AssistantContent::text(text)),
        });
    }

    pub(crate) async fn emit_event(&self, ctx: &TurnContext, msg: EventMsg) {
        let _event = Event {
            id: ctx.sub_id.clone(),
            msg,
        };
    }

    pub(crate) async fn set_agent_status(&self, status: AgentStatus) {
        self.agent_status.send_replace(status);
    }

    pub(crate) fn model(&self) -> &SessionModel {
        &self.model
    }

    fn next_internal_sub_id(&self) -> String {
        self.next_internal_sub_id
            .fetch_add(1, Ordering::Relaxed)
            .to_string()
    }
}
