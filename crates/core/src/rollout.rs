use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use cazean_protocol::{EventMsg, ProjectInstructions, ThreadId, TurnInterruptedEvent};
use rig::{
    OneOrMany,
    message::{AssistantContent, Message, Reasoning as MessageReasoning, Text, UserContent},
};
use serde::{Deserialize, Serialize};
use time::{
    OffsetDateTime, format_description::FormatItem, format_description::well_known::Rfc3339,
    macros::format_description,
};
use tokio::{
    fs::{self, File},
    io::AsyncWriteExt,
    sync::Mutex,
};

use crate::agent::subagent_result::{CompletionEntry, completion_entries_to_user_message};

#[derive(Debug, Clone)]
pub(crate) struct RolloutRecorder {
    path: PathBuf,
    file: Arc<Mutex<File>>,
}

#[derive(Debug, Clone)]
pub struct ThreadSummary {
    pub thread_id: ThreadId,
    pub rollout_path: PathBuf,
    pub created_at: String,
    pub updated_at: String,
    pub last_user_message: Option<String>,
    pub last_assistant_message: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ResumeState {
    pub thread_id: ThreadId,
    pub history: Vec<Message>,
    pub initial_messages: Vec<EventMsg>,
    pub next_turn_index: u64,
    pub project_instructions: Option<ProjectInstructions>,
    /// Plan-mode state at the time the rollout was written (the last persisted
    /// `PlanModeChanged` wins), so a thread resumed mid-planning keeps its
    /// restricted tool set.
    pub plan_mode: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub(crate) enum HistoryMessage {
    Full {
        message: Message,
    },
    /// A deferred subagent batch's completions. Stored as the typed source of
    /// truth; the model-facing `Message::User` (one `agent_completed` JSON text
    /// item per entry) is reconstructed from it on resume via
    /// [`completion_entries_to_user_message`], byte-identical to the live turn.
    SubagentCompletion {
        completions: Vec<CompletionEntry>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionMeta {
    pub thread_id: ThreadId,
    pub cwd: PathBuf,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_instructions: Option<ProjectInstructions>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum PersistedItem {
    SessionMeta(SessionMeta),
    HistoryMessage(HistoryMessage),
    UserMessage { text: String },
    Event(EventMsg),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RolloutEnvelope {
    timestamp: String,
    item: PersistedItem,
}

impl RolloutRecorder {
    #[cfg(test)]
    pub(crate) async fn create(
        workspace_root: &Path,
        thread_id: ThreadId,
        cwd: &Path,
    ) -> Result<Self> {
        Self::create_with_project_instructions(workspace_root, thread_id, cwd, None).await
    }

    pub(crate) async fn create_with_project_instructions(
        workspace_root: &Path,
        thread_id: ThreadId,
        cwd: &Path,
        project_instructions: Option<ProjectInstructions>,
    ) -> Result<Self> {
        let path = create_rollout_path(workspace_root, thread_id)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        let recorder = Self {
            path,
            file: Arc::new(Mutex::new(file)),
        };
        recorder
            .append(PersistedItem::SessionMeta(SessionMeta {
                thread_id,
                cwd: cwd.to_path_buf(),
                created_at: now_rfc3339()?,
                project_instructions,
            }))
            .await?;
        Ok(recorder)
    }

    pub(crate) async fn resume(path: PathBuf) -> Result<Self> {
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        Ok(Self {
            path,
            file: Arc::new(Mutex::new(file)),
        })
    }

    pub(crate) fn path(&self) -> &Path {
        self.path.as_path()
    }

    pub(crate) async fn append(&self, item: PersistedItem) -> Result<()> {
        let envelope = RolloutEnvelope {
            timestamp: now_rfc3339()?,
            item,
        };
        let mut line = serde_json::to_vec(&envelope)?;
        line.push(b'\n');
        let mut file = self.file.lock().await;
        file.write_all(&line).await?;
        file.flush().await?;
        Ok(())
    }
}

fn persist_event(event: &EventMsg) -> bool {
    matches!(
        event,
        EventMsg::SessionConfigured(_)
            | EventMsg::TurnStarted(_)
            | EventMsg::TurnCompleted(_)
            | EventMsg::TurnInterrupted(_)
            | EventMsg::AgentMessageCompleted(_)
            | EventMsg::AgentReasoningCompleted(_)
            | EventMsg::ToolCallStarted(_)
            | EventMsg::ToolCallCompleted(_)
            | EventMsg::PlanModeChanged(_)
            | EventMsg::Error(_)
    )
}

pub(crate) fn persisted_event_item(event: &EventMsg) -> Option<PersistedItem> {
    match event {
        EventMsg::UserMessage { text } => Some(PersistedItem::UserMessage { text: text.clone() }),
        event if persist_event(event) => Some(PersistedItem::Event(event.clone())),
        _ => None,
    }
}

/// How `load_state` should treat a rollout whose last turn is still open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecoveryMode {
    /// The thread is being brought back to life (or inspected post-mortem):
    /// synthesize a trailing `TurnInterrupted` so a crashed open turn is
    /// surfaced as interrupted.
    Resume,
    /// The thread is still running elsewhere; an open turn is live, not
    /// crashed, so replay events verbatim.
    PreviewLive,
}

pub(crate) async fn load_resume_state(path: &Path) -> Result<ResumeState> {
    load_state(path, RecoveryMode::Resume).await
}

fn user_history_message(text: String) -> Message {
    Message::User {
        content: OneOrMany::one(UserContent::Text(Text {
            text,
            additional_params: None,
        })),
    }
}

fn assistant_reasoning_history_message(reasoning: Vec<(String, String)>) -> Option<Message> {
    let content = reasoning
        .into_iter()
        .filter(|(_, text)| !text.is_empty())
        .map(|(id, text)| {
            AssistantContent::Reasoning(MessageReasoning::summaries(vec![text]).with_id(id))
        })
        .collect::<Vec<_>>();
    Some(Message::Assistant {
        id: None,
        content: OneOrMany::many(content).ok()?,
    })
}

fn remove_synthetic_interrupted_prompt(
    history: &mut Vec<Message>,
    interrupted_synthetic_start: &mut Option<usize>,
) {
    if let Some(start) = interrupted_synthetic_start.take() {
        history.truncate(start);
    }
}

pub(crate) async fn load_state(path: &Path, recovery: RecoveryMode) -> Result<ResumeState> {
    let contents = fs::read_to_string(path).await?;
    let mut meta: Option<SessionMeta> = None;
    let mut history = Vec::new();
    let mut initial_messages = Vec::new();
    let mut max_turn_index = None::<u64>;
    let mut has_open_turn = false;
    let mut has_terminal_turn = false;
    let mut open_turn_history_start = None::<usize>;
    let mut open_turn_user_message = None::<String>;
    let mut open_turn_reasoning = Vec::<(String, String)>::new();
    let mut interrupted_synthetic_start = None::<usize>;
    let mut unstable_history_start = None::<usize>;
    let mut plan_mode = false;

    for line in contents.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let envelope = match serde_json::from_str::<RolloutEnvelope>(line) {
            Ok(envelope) => envelope,
            Err(_) => continue,
        };
        match envelope.item {
            PersistedItem::SessionMeta(session_meta) => {
                if meta.is_none() {
                    meta = Some(session_meta);
                }
            }
            PersistedItem::HistoryMessage(HistoryMessage::Full { message }) => {
                remove_synthetic_interrupted_prompt(&mut history, &mut interrupted_synthetic_start);
                history.push(message);
            }
            PersistedItem::HistoryMessage(HistoryMessage::SubagentCompletion { completions }) => {
                // Reconstruct the model-facing user message the live turn built,
                // restoring it into provider history. Like `Full`, it carries no
                // transcript row (deferred completions are transcript-silent), so
                // it does not push to `initial_messages`.
                if let Some(message) = completion_entries_to_user_message(&completions) {
                    remove_synthetic_interrupted_prompt(
                        &mut history,
                        &mut interrupted_synthetic_start,
                    );
                    history.push(message);
                }
            }
            PersistedItem::UserMessage { text } => {
                if open_turn_history_start.is_some() && open_turn_user_message.is_none() {
                    open_turn_user_message = Some(text.clone());
                }
                initial_messages.push(EventMsg::UserMessage { text });
            }
            PersistedItem::Event(event) => {
                match &event {
                    EventMsg::TurnStarted(_) => {
                        open_turn_history_start = Some(history.len());
                        open_turn_user_message = None;
                        open_turn_reasoning.clear();
                        interrupted_synthetic_start = None;
                    }
                    EventMsg::TurnCompleted(_) => {
                        open_turn_history_start = None;
                        open_turn_user_message = None;
                        open_turn_reasoning.clear();
                        interrupted_synthetic_start = None;
                        unstable_history_start = None;
                    }
                    EventMsg::TurnInterrupted(_) => {
                        if let Some(start) = open_turn_history_start.take()
                            && history.len() == start
                        {
                            interrupted_synthetic_start = Some(history.len());
                            if let Some(text) = open_turn_user_message.take() {
                                history.push(user_history_message(text));
                            }
                            if let Some(message) = assistant_reasoning_history_message(
                                std::mem::take(&mut open_turn_reasoning),
                            ) {
                                history.push(message);
                            }
                        }
                        open_turn_user_message = None;
                        open_turn_reasoning.clear();
                    }
                    EventMsg::Error(_) => {
                        let start = open_turn_history_start.take().unwrap_or(history.len());
                        open_turn_user_message = None;
                        open_turn_reasoning.clear();
                        if unstable_history_start.is_none() {
                            unstable_history_start = Some(start);
                        }
                    }
                    EventMsg::AgentReasoningCompleted(reasoning)
                        if open_turn_history_start.is_some() =>
                    {
                        open_turn_reasoning
                            .push((reasoning.item_id.clone(), reasoning.text.clone()));
                    }
                    _ => {}
                }
                update_turn_tracking(
                    &event,
                    &mut max_turn_index,
                    &mut has_open_turn,
                    &mut has_terminal_turn,
                );
                if let EventMsg::PlanModeChanged(change) = &event {
                    plan_mode = change.enabled;
                }
                initial_messages.push(event);
            }
        }
    }

    let meta = meta.with_context(|| format!("missing session metadata in {}", path.display()))?;
    if recovery == RecoveryMode::Resume {
        let truncate_to = if has_open_turn && !has_terminal_turn {
            unstable_history_start.or(open_turn_history_start)
        } else {
            unstable_history_start
        };
        if let Some(start) = truncate_to {
            history.truncate(start);
        }
    }
    if recovery == RecoveryMode::Resume && has_open_turn && !has_terminal_turn {
        initial_messages.push(EventMsg::TurnInterrupted(TurnInterruptedEvent {
            thread_id: meta.thread_id.to_string(),
            turn_id: max_turn_index.unwrap_or(0).to_string(),
            reason: "resume_recovery".to_string(),
        }));
    }

    Ok(ResumeState {
        thread_id: meta.thread_id,
        history,
        initial_messages,
        next_turn_index: max_turn_index.map_or(0, |value| value.saturating_add(1)),
        project_instructions: meta.project_instructions,
        plan_mode,
    })
}

pub(crate) async fn list_threads(workspace_root: &Path) -> Result<Vec<ThreadSummary>> {
    let mut rollout_paths = Vec::new();
    let sessions_root = sessions_root(workspace_root);
    if fs::try_exists(&sessions_root).await.unwrap_or(false) {
        collect_rollout_paths(&sessions_root, &mut rollout_paths).await?;
    }

    let mut threads = Vec::new();
    for path in rollout_paths {
        if let Ok(summary) = summarize_rollout(&path).await {
            threads.push(summary);
        }
    }
    threads.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
    Ok(threads)
}

pub(crate) async fn find_thread_path(
    workspace_root: &Path,
    thread_id: ThreadId,
) -> Result<PathBuf> {
    let threads = list_threads(workspace_root).await?;
    threads
        .into_iter()
        .find(|thread| thread.thread_id == thread_id)
        .map(|thread| thread.rollout_path)
        .with_context(|| format!("unknown thread id: {thread_id}"))
}

fn update_turn_tracking(
    event: &EventMsg,
    max_turn_index: &mut Option<u64>,
    has_open_turn: &mut bool,
    has_terminal_turn: &mut bool,
) {
    match event {
        EventMsg::TurnStarted(turn) => {
            if let Ok(turn_index) = turn.turn_id.parse::<u64>() {
                *max_turn_index =
                    Some(max_turn_index.map_or(turn_index, |current| current.max(turn_index)));
            }
            *has_open_turn = true;
            *has_terminal_turn = false;
        }
        EventMsg::TurnCompleted(_) | EventMsg::TurnInterrupted(_) | EventMsg::Error(_) => {
            *has_open_turn = false;
            *has_terminal_turn = true;
        }
        _ => {}
    }
}

fn update_assistant_summary_message(
    message: &Message,
    last_assistant_message: &mut Option<String>,
) {
    match message {
        Message::Assistant { content, .. } => {
            if let Some(text) = assistant_text(content) {
                *last_assistant_message = Some(text);
            }
        }
        Message::System { .. } | Message::User { .. } => {}
    }
}

fn assistant_text(content: &OneOrMany<AssistantContent>) -> Option<String> {
    let text = content
        .iter()
        .filter_map(|content| match content {
            AssistantContent::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<String>();
    (!text.is_empty()).then_some(text)
}

async fn summarize_rollout(path: &Path) -> Result<ThreadSummary> {
    let contents = fs::read_to_string(path).await?;
    let mut meta: Option<SessionMeta> = None;
    let mut updated_at = None::<String>;
    let mut last_user_message = None;
    let mut last_assistant_message = None;

    for line in contents.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let envelope = match serde_json::from_str::<RolloutEnvelope>(line) {
            Ok(envelope) => envelope,
            Err(_) => continue,
        };
        updated_at = Some(envelope.timestamp.clone());
        match envelope.item {
            PersistedItem::SessionMeta(session_meta) => {
                if meta.is_none() {
                    meta = Some(session_meta);
                }
            }
            PersistedItem::HistoryMessage(HistoryMessage::Full { message }) => {
                update_assistant_summary_message(&message, &mut last_assistant_message);
            }
            PersistedItem::HistoryMessage(HistoryMessage::SubagentCompletion { .. }) => {
                // A subagent completion is internal tool context, not the
                // parent's own speech: it must not surface as the thread's
                // `last_user_message` preview (nor as an assistant summary).
            }
            PersistedItem::UserMessage { text } => {
                last_user_message = Some(text);
            }
            PersistedItem::Event(_) => {}
        }
    }

    let meta = meta.with_context(|| format!("missing session metadata in {}", path.display()))?;
    let created_at = meta.created_at.clone();
    Ok(ThreadSummary {
        thread_id: meta.thread_id,
        rollout_path: path.to_path_buf(),
        created_at: created_at.clone(),
        updated_at: updated_at.unwrap_or(created_at),
        last_user_message,
        last_assistant_message,
    })
}

async fn collect_rollout_paths(dir: &Path, paths: &mut Vec<PathBuf>) -> Result<()> {
    let mut entries = fs::read_dir(dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        let file_type = entry.file_type().await?;
        if file_type.is_dir() {
            Box::pin(collect_rollout_paths(&path, paths)).await?;
        } else if path.extension().is_some_and(|ext| ext == "jsonl") {
            paths.push(path);
        }
    }
    Ok(())
}

fn create_rollout_path(workspace_root: &Path, thread_id: ThreadId) -> Result<PathBuf> {
    let now = OffsetDateTime::now_local().unwrap_or_else(|_| OffsetDateTime::now_utc());
    let dir = sessions_root(workspace_root)
        .join(now.year().to_string())
        .join(format!("{:02}", u8::from(now.month())))
        .join(format!("{:02}", now.day()));
    let file_format: &[FormatItem<'static>] =
        format_description!("[year]-[month]-[day]T[hour]-[minute]-[second]");
    let timestamp = now.format(file_format)?;
    Ok(dir.join(format!("rollout-{timestamp}-{thread_id}.jsonl")))
}

fn sessions_root(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".cazean").join("sessions")
}

fn now_rfc3339() -> Result<String> {
    Ok(OffsetDateTime::now_utc().format(&Rfc3339)?)
}

pub(crate) fn workspace_root() -> Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    if !cwd.is_dir() {
        bail!("workspace root is not a directory: {}", cwd.display());
    }
    Ok(cwd)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cazean_protocol::{
        AgentPath, AgentReasoningCompletedEvent, AgentStatus, ErrorEvent, ErrorInfo,
        ProjectInstructionEntry, ProjectInstructions, SessionConfiguredEvent, TurnStartedEvent,
    };
    use rig::message::{ReasoningContent, Text, UserContent};

    fn test_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("cazean-rollout-{name}-{}", ThreadId::new()))
    }

    fn user_message_text(message: &Message) -> Option<String> {
        let Message::User { content } = message else {
            return None;
        };
        let text = content
            .iter()
            .filter_map(|content| match content {
                UserContent::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<String>();
        (!text.is_empty()).then_some(text)
    }

    fn assistant_message_text(message: &Message) -> Option<String> {
        let Message::Assistant { content, .. } = message else {
            return None;
        };
        let text = content
            .iter()
            .filter_map(|content| match content {
                AssistantContent::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect::<String>();
        (!text.is_empty()).then_some(text)
    }

    fn assistant_reasoning_summaries(message: &Message) -> Vec<(Option<String>, String)> {
        let Message::Assistant { content, .. } = message else {
            return Vec::new();
        };
        content
            .iter()
            .filter_map(|content| match content {
                AssistantContent::Reasoning(reasoning) => Some((
                    reasoning.id.clone(),
                    reasoning
                        .content
                        .iter()
                        .filter_map(|content| match content {
                            ReasoningContent::Summary(text) => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<String>(),
                )),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn create_and_list_rollout_threads() -> Result<()> {
        let root = test_root("list");
        let cwd = root.join("workspace");
        fs::create_dir_all(&cwd).await?;

        let thread_id = ThreadId::new();
        let recorder = RolloutRecorder::create(&root, thread_id, &cwd).await?;
        recorder
            .append(PersistedItem::UserMessage {
                text: "hello".to_string(),
            })
            .await?;

        let threads = list_threads(&root).await?;
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].thread_id, thread_id);
        assert_eq!(threads[0].last_user_message.as_deref(), Some("hello"));

        let _ = fs::remove_dir_all(&root).await;
        Ok(())
    }

    #[tokio::test]
    async fn thread_summary_uses_user_message_event_not_internal_full_user_text() -> Result<()> {
        let root = test_root("list-user-event");
        let cwd = root.join("workspace");
        fs::create_dir_all(&cwd).await?;

        let thread_id = ThreadId::new();
        let recorder = RolloutRecorder::create(&root, thread_id, &cwd).await?;
        recorder
            .append(PersistedItem::HistoryMessage(HistoryMessage::Full {
                message: Message::User {
                    content: OneOrMany::one(UserContent::Text(Text {
                        text: "human prompt".to_string(),
                        additional_params: None,
                    })),
                },
            }))
            .await?;
        recorder
            .append(
                persisted_event_item(&EventMsg::UserMessage {
                    text: "human prompt".to_string(),
                })
                .with_context(|| "user message should persist")?,
            )
            .await?;
        recorder
            .append(PersistedItem::HistoryMessage(HistoryMessage::Full {
                message: Message::User {
                    content: OneOrMany::one(UserContent::Text(Text {
                        text: r#"{"event":"agent_completed","last_assistant_message":"internal"}"#
                            .to_string(),
                        additional_params: None,
                    })),
                },
            }))
            .await?;

        let threads = list_threads(&root).await?;
        assert_eq!(threads.len(), 1);
        assert_eq!(
            threads[0].last_user_message.as_deref(),
            Some("human prompt")
        );

        let _ = fs::remove_dir_all(&root).await;
        Ok(())
    }

    #[tokio::test]
    async fn resume_state_restores_last_plan_mode() -> Result<()> {
        let root = test_root("plan-mode");
        let cwd = root.join("workspace");
        fs::create_dir_all(&cwd).await?;

        let thread_id = ThreadId::new();
        let recorder = RolloutRecorder::create(&root, thread_id, &cwd).await?;

        let state = load_resume_state(recorder.path()).await?;
        assert!(!state.plan_mode, "fresh rollout should not be in plan mode");

        let enabled = EventMsg::PlanModeChanged(cazean_protocol::PlanModeChangedEvent {
            thread_id: thread_id.to_string(),
            enabled: true,
        });
        recorder
            .append(persisted_event_item(&enabled).with_context(|| "plan mode should persist")?)
            .await?;
        let state = load_resume_state(recorder.path()).await?;
        assert!(state.plan_mode, "last PlanModeChanged(true) should win");

        let disabled = EventMsg::PlanModeChanged(cazean_protocol::PlanModeChangedEvent {
            thread_id: thread_id.to_string(),
            enabled: false,
        });
        recorder
            .append(persisted_event_item(&disabled).with_context(|| "plan mode should persist")?)
            .await?;
        let state = load_resume_state(recorder.path()).await?;
        assert!(!state.plan_mode, "an on->off sequence resolves to off");

        let _ = fs::remove_dir_all(&root).await;
        Ok(())
    }

    #[tokio::test]
    async fn resume_state_reconstructs_history_and_recovery_interrupt() -> Result<()> {
        let root = test_root("resume");
        let cwd = root.join("workspace");
        fs::create_dir_all(&cwd).await?;

        let thread_id = ThreadId::new();
        let recorder = RolloutRecorder::create(&root, thread_id, &cwd).await?;
        recorder
            .append(PersistedItem::Event(EventMsg::SessionConfigured(
                SessionConfiguredEvent {
                    thread_id: thread_id.to_string(),
                    rollout_path: Some(recorder.path().display().to_string()),
                },
            )))
            .await?;
        recorder
            .append(PersistedItem::HistoryMessage(HistoryMessage::Full {
                message: Message::User {
                    content: OneOrMany::one(UserContent::Text(Text {
                        text: "hello".to_string(),
                        additional_params: None,
                    })),
                },
            }))
            .await?;
        recorder
            .append(PersistedItem::HistoryMessage(HistoryMessage::Full {
                message: Message::Assistant {
                    id: None,
                    content: OneOrMany::one(AssistantContent::text("world".to_string())),
                },
            }))
            .await?;
        recorder
            .append(PersistedItem::Event(EventMsg::TurnStarted(
                TurnStartedEvent {
                    thread_id: thread_id.to_string(),
                    turn_id: "4".to_string(),
                },
            )))
            .await?;

        let state = load_resume_state(recorder.path()).await?;
        assert_eq!(state.thread_id, thread_id);
        assert_eq!(state.next_turn_index, 5);
        assert_eq!(state.history.len(), 2);
        assert!(matches!(state.history[0], Message::User { .. }));
        assert!(matches!(state.history[1], Message::Assistant { .. }));
        assert!(matches!(
            state.initial_messages.last(),
            Some(EventMsg::TurnInterrupted(turn)) if turn.reason == "resume_recovery"
        ));

        let _ = fs::remove_dir_all(&root).await;
        Ok(())
    }

    #[tokio::test]
    async fn resume_state_preserves_model_history_from_interrupted_turn() -> Result<()> {
        let root = test_root("resume-interrupted-turn");
        let cwd = root.join("workspace");
        fs::create_dir_all(&cwd).await?;

        let thread_id = ThreadId::new();
        let recorder = RolloutRecorder::create(&root, thread_id, &cwd).await?;
        recorder
            .append(PersistedItem::HistoryMessage(HistoryMessage::Full {
                message: Message::User {
                    content: OneOrMany::one(UserContent::Text(Text {
                        text: "stable prompt".to_string(),
                        additional_params: None,
                    })),
                },
            }))
            .await?;
        recorder
            .append(PersistedItem::HistoryMessage(HistoryMessage::Full {
                message: Message::Assistant {
                    id: None,
                    content: OneOrMany::one(AssistantContent::text("stable answer".to_string())),
                },
            }))
            .await?;
        recorder
            .append(PersistedItem::Event(EventMsg::TurnStarted(
                TurnStartedEvent {
                    thread_id: thread_id.to_string(),
                    turn_id: "3".to_string(),
                },
            )))
            .await?;
        recorder
            .append(PersistedItem::HistoryMessage(HistoryMessage::Full {
                message: Message::User {
                    content: OneOrMany::one(UserContent::Text(Text {
                        text: "make a plan".to_string(),
                        additional_params: None,
                    })),
                },
            }))
            .await?;
        recorder
            .append(PersistedItem::HistoryMessage(HistoryMessage::Full {
                message: Message::Assistant {
                    id: None,
                    content: OneOrMany::one(AssistantContent::text("partial plan".to_string())),
                },
            }))
            .await?;
        recorder
            .append(PersistedItem::Event(EventMsg::TurnInterrupted(
                TurnInterruptedEvent {
                    thread_id: thread_id.to_string(),
                    turn_id: "3".to_string(),
                    reason: "interrupted".to_string(),
                },
            )))
            .await?;
        recorder
            .append(PersistedItem::HistoryMessage(HistoryMessage::Full {
                message: Message::Assistant {
                    id: None,
                    content: OneOrMany::one(AssistantContent::text(
                        "late partial after interruption event".to_string(),
                    )),
                },
            }))
            .await?;

        let state = load_resume_state(recorder.path()).await?;
        assert_eq!(state.next_turn_index, 4);
        assert_eq!(state.history.len(), 5);
        assert_eq!(
            user_message_text(&state.history[0]).as_deref(),
            Some("stable prompt")
        );
        assert_eq!(
            assistant_message_text(&state.history[1]).as_deref(),
            Some("stable answer")
        );
        assert_eq!(
            user_message_text(&state.history[2]).as_deref(),
            Some("make a plan")
        );
        assert_eq!(
            assistant_message_text(&state.history[3]).as_deref(),
            Some("partial plan")
        );
        assert_eq!(
            assistant_message_text(&state.history[4]).as_deref(),
            Some("late partial after interruption event")
        );
        assert!(state.initial_messages.iter().any(|event| {
            matches!(event, EventMsg::TurnInterrupted(turn) if turn.reason == "interrupted")
        }));

        let _ = fs::remove_dir_all(&root).await;
        Ok(())
    }

    #[tokio::test]
    async fn resume_state_synthesizes_interrupted_prompt_when_cleanup_did_not_persist_tail()
    -> Result<()> {
        let root = test_root("resume-interrupted-synthetic-prompt");
        let cwd = root.join("workspace");
        fs::create_dir_all(&cwd).await?;

        let thread_id = ThreadId::new();
        let recorder = RolloutRecorder::create(&root, thread_id, &cwd).await?;
        recorder
            .append(PersistedItem::Event(EventMsg::TurnStarted(
                TurnStartedEvent {
                    thread_id: thread_id.to_string(),
                    turn_id: "0".to_string(),
                },
            )))
            .await?;
        recorder
            .append(
                persisted_event_item(&EventMsg::UserMessage {
                    text: "make a plan to refactor plan mode".to_string(),
                })
                .with_context(|| "user message should persist")?,
            )
            .await?;
        recorder
            .append(PersistedItem::Event(EventMsg::AgentReasoningCompleted(
                AgentReasoningCompletedEvent {
                    thread_id: thread_id.to_string(),
                    turn_id: "0".to_string(),
                    item_id: "rs_planning".to_string(),
                    text: "**Planning for Refactoring**\n\nNeed to inspect plan-mode code."
                        .to_string(),
                },
            )))
            .await?;
        recorder
            .append(PersistedItem::Event(EventMsg::TurnInterrupted(
                TurnInterruptedEvent {
                    thread_id: thread_id.to_string(),
                    turn_id: "0".to_string(),
                    reason: "interrupted".to_string(),
                },
            )))
            .await?;
        recorder
            .append(PersistedItem::Event(EventMsg::TurnStarted(
                TurnStartedEvent {
                    thread_id: thread_id.to_string(),
                    turn_id: "1".to_string(),
                },
            )))
            .await?;
        recorder
            .append(
                persisted_event_item(&EventMsg::UserMessage {
                    text: "continue".to_string(),
                })
                .with_context(|| "user message should persist")?,
            )
            .await?;
        recorder
            .append(PersistedItem::HistoryMessage(HistoryMessage::Full {
                message: Message::User {
                    content: OneOrMany::one(UserContent::Text(Text {
                        text: "continue".to_string(),
                        additional_params: None,
                    })),
                },
            }))
            .await?;
        recorder
            .append(PersistedItem::HistoryMessage(HistoryMessage::Full {
                message: Message::Assistant {
                    id: None,
                    content: OneOrMany::one(AssistantContent::text("continued plan".to_string())),
                },
            }))
            .await?;
        recorder
            .append(PersistedItem::Event(EventMsg::TurnCompleted(
                cazean_protocol::TurnCompletedEvent {
                    thread_id: thread_id.to_string(),
                    turn_id: "1".to_string(),
                    last_assistant_message: Some("continued plan".to_string()),
                },
            )))
            .await?;

        let state = load_resume_state(recorder.path()).await?;
        assert_eq!(state.next_turn_index, 2);
        assert_eq!(state.history.len(), 4);
        assert_eq!(
            user_message_text(&state.history[0]).as_deref(),
            Some("make a plan to refactor plan mode")
        );
        assert_eq!(
            assistant_reasoning_summaries(&state.history[1]),
            vec![(
                Some("rs_planning".to_string()),
                "**Planning for Refactoring**\n\nNeed to inspect plan-mode code.".to_string(),
            ),]
        );
        assert_eq!(
            user_message_text(&state.history[2]).as_deref(),
            Some("continue")
        );
        assert_eq!(
            assistant_message_text(&state.history[3]).as_deref(),
            Some("continued plan")
        );

        let _ = fs::remove_dir_all(&root).await;
        Ok(())
    }

    #[tokio::test]
    async fn resume_state_prunes_model_history_from_errored_open_turn() -> Result<()> {
        let root = test_root("resume-errored-turn");
        let cwd = root.join("workspace");
        fs::create_dir_all(&cwd).await?;

        let thread_id = ThreadId::new();
        let recorder = RolloutRecorder::create(&root, thread_id, &cwd).await?;
        recorder
            .append(PersistedItem::HistoryMessage(HistoryMessage::Full {
                message: Message::User {
                    content: OneOrMany::one(UserContent::Text(Text {
                        text: "stable prompt".to_string(),
                        additional_params: None,
                    })),
                },
            }))
            .await?;
        recorder
            .append(PersistedItem::HistoryMessage(HistoryMessage::Full {
                message: Message::Assistant {
                    id: None,
                    content: OneOrMany::one(AssistantContent::text("stable answer".to_string())),
                },
            }))
            .await?;
        recorder
            .append(PersistedItem::Event(EventMsg::TurnStarted(
                TurnStartedEvent {
                    thread_id: thread_id.to_string(),
                    turn_id: "7".to_string(),
                },
            )))
            .await?;
        recorder
            .append(PersistedItem::HistoryMessage(HistoryMessage::Full {
                message: Message::User {
                    content: OneOrMany::one(UserContent::Text(Text {
                        text: "continue".to_string(),
                        additional_params: None,
                    })),
                },
            }))
            .await?;
        recorder
            .append(PersistedItem::Event(EventMsg::Error(ErrorEvent {
                error: ErrorInfo::new("turn_failed", "provider reset"),
            })))
            .await?;

        let state = load_resume_state(recorder.path()).await?;
        assert_eq!(state.next_turn_index, 8);
        assert_eq!(state.history.len(), 2);
        assert_eq!(
            user_message_text(&state.history[0]).as_deref(),
            Some("stable prompt")
        );
        assert!(matches!(state.history[1], Message::Assistant { .. }));
        assert!(
            state
                .initial_messages
                .iter()
                .any(|event| matches!(event, EventMsg::Error(_)))
        );
        assert!(!state.initial_messages.iter().any(|event| {
            matches!(event, EventMsg::TurnInterrupted(turn) if turn.reason == "resume_recovery")
        }));

        let _ = fs::remove_dir_all(&root).await;
        Ok(())
    }

    #[tokio::test]
    async fn resume_reconstructs_subagent_completion_into_model_history() -> Result<()> {
        let root = test_root("resume-subagent-completion");
        let cwd = root.join("workspace");
        fs::create_dir_all(&cwd).await?;

        let thread_id = ThreadId::new();
        let recorder = RolloutRecorder::create(&root, thread_id, &cwd).await?;
        recorder
            .append(PersistedItem::HistoryMessage(HistoryMessage::Full {
                message: Message::User {
                    content: OneOrMany::one(UserContent::Text(Text {
                        text: "spawn two agents".to_string(),
                        additional_params: None,
                    })),
                },
            }))
            .await?;
        // A grouped deferred completion: two entries sharing one user message,
        // exactly as a mixed-batch flush persists them.
        let completions = vec![
            CompletionEntry {
                child_thread_id: Some(ThreadId::new()),
                agent_path: AgentPath::root().join("alpha")?,
                agent_nickname: Some("alpha".to_string()),
                status: AgentStatus::Completed(Some("alpha done".to_string())),
                last_assistant_message: None,
            },
            CompletionEntry {
                child_thread_id: Some(ThreadId::new()),
                agent_path: AgentPath::root().join("beta")?,
                agent_nickname: Some("beta".to_string()),
                status: AgentStatus::Completed(Some("beta done".to_string())),
                last_assistant_message: Some("explicit override".to_string()),
            },
        ];
        recorder
            .append(PersistedItem::HistoryMessage(
                HistoryMessage::SubagentCompletion {
                    completions: completions.clone(),
                },
            ))
            .await?;

        let state = load_resume_state(recorder.path()).await?;
        // Reconstructs into provider history (prompt + the completion message),
        // never into the visible transcript.
        assert_eq!(state.history.len(), 2);
        assert!(matches!(state.history[0], Message::User { .. }));
        let Some(Message::User { content }) = state.history.get(1) else {
            panic!("expected the reconstructed subagent-completion user message");
        };
        let reconstructed_texts = content
            .iter()
            .filter_map(|item| match item {
                UserContent::Text(Text { text, .. }) => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        // Byte-identical to what the live turn would have produced (one
        // `agent_completed` JSON text item per entry, in order).
        let expected_texts = completions
            .iter()
            .map(CompletionEntry::to_model_json)
            .collect::<Vec<_>>();
        assert_eq!(reconstructed_texts, expected_texts);
        assert!(
            !state
                .initial_messages
                .iter()
                .any(|event| matches!(event, EventMsg::UserMessage { .. }))
        );

        let _ = fs::remove_dir_all(&root).await;
        Ok(())
    }

    #[tokio::test]
    async fn thread_summary_ignores_subagent_completion_record() -> Result<()> {
        let root = test_root("list-subagent-completion");
        let cwd = root.join("workspace");
        fs::create_dir_all(&cwd).await?;

        let thread_id = ThreadId::new();
        let recorder = RolloutRecorder::create(&root, thread_id, &cwd).await?;
        recorder
            .append(
                persisted_event_item(&EventMsg::UserMessage {
                    text: "human prompt".to_string(),
                })
                .with_context(|| "user message should persist")?,
            )
            .await?;
        recorder
            .append(PersistedItem::HistoryMessage(
                HistoryMessage::SubagentCompletion {
                    completions: vec![CompletionEntry {
                        child_thread_id: Some(ThreadId::new()),
                        agent_path: AgentPath::root().join("alpha")?,
                        agent_nickname: Some("alpha".to_string()),
                        status: AgentStatus::Completed(Some("internal".to_string())),
                        last_assistant_message: Some("internal".to_string()),
                    }],
                },
            ))
            .await?;

        let threads = list_threads(&root).await?;
        assert_eq!(threads.len(), 1);
        assert_eq!(
            threads[0].last_user_message.as_deref(),
            Some("human prompt")
        );

        let _ = fs::remove_dir_all(&root).await;
        Ok(())
    }

    #[tokio::test]
    async fn old_session_meta_without_project_instructions_resumes() -> Result<()> {
        let root = test_root("old-meta");
        let cwd = root.join("workspace");
        fs::create_dir_all(&cwd).await?;

        let thread_id = ThreadId::new();
        let path = root.join("old-session.jsonl");
        let envelope = serde_json::json!({
            "timestamp": "2026-01-01T00:00:00Z",
            "item": {
                "kind": "session_meta",
                "threadId": thread_id,
                "cwd": cwd,
                "createdAt": "2026-01-01T00:00:00Z",
            },
        });
        fs::write(&path, format!("{envelope}\n")).await?;

        let state = load_resume_state(&path).await?;
        assert_eq!(state.thread_id, thread_id);
        assert_eq!(state.project_instructions, None);

        let _ = fs::remove_dir_all(&root).await;
        Ok(())
    }

    #[tokio::test]
    async fn project_instructions_persist_and_restore_from_session_meta() -> Result<()> {
        let root = test_root("project-instructions");
        let cwd = root.join("workspace");
        fs::create_dir_all(&cwd).await?;

        let thread_id = ThreadId::new();
        let instructions = ProjectInstructions {
            entries: vec![ProjectInstructionEntry {
                source_path: cwd.join("AGENTS.md").display().to_string(),
                directory: cwd.display().to_string(),
                text: "Persisted instructions".to_string(),
            }],
        };
        let recorder = RolloutRecorder::create_with_project_instructions(
            &root,
            thread_id,
            &cwd,
            Some(instructions.clone()),
        )
        .await?;

        let contents = fs::read_to_string(recorder.path()).await?;
        assert!(contents.contains("projectInstructions"));

        let state = load_resume_state(recorder.path()).await?;
        assert_eq!(state.project_instructions, Some(instructions));

        let _ = fs::remove_dir_all(&root).await;
        Ok(())
    }

    #[tokio::test]
    async fn full_assistant_message_round_trips() -> Result<()> {
        let root = test_root("full-assistant");
        let cwd = root.join("workspace");
        fs::create_dir_all(&cwd).await?;

        let thread_id = ThreadId::new();
        let recorder = RolloutRecorder::create(&root, thread_id, &cwd).await?;
        let reasoning = rig::message::Reasoning::new("thinking").with_id("rs_1".to_string());
        let message = Message::Assistant {
            id: Some("assistant_1".to_string()),
            content: OneOrMany::many(vec![
                AssistantContent::Reasoning(reasoning),
                AssistantContent::text("answer"),
            ])?,
        };
        recorder
            .append(PersistedItem::HistoryMessage(HistoryMessage::Full {
                message: message.clone(),
            }))
            .await?;

        let state = load_resume_state(recorder.path()).await?;
        // rig 0.38's `Text::additional_params` is `#[serde(flatten)]`, so an empty
        // one round-trips through JSON as `Some({})` rather than `None`. That is
        // wire-identical, so compare serialized forms instead of Rust `PartialEq`.
        assert_eq!(
            serde_json::to_value(&state.history)?,
            serde_json::to_value(vec![message])?,
        );

        let _ = fs::remove_dir_all(&root).await;
        Ok(())
    }

    #[tokio::test]
    async fn encrypted_reasoning_survives_load_resume_state() -> Result<()> {
        let root = test_root("encrypted-reasoning");
        let cwd = root.join("workspace");
        fs::create_dir_all(&cwd).await?;

        let thread_id = ThreadId::new();
        let recorder = RolloutRecorder::create(&root, thread_id, &cwd).await?;
        let encrypted =
            rig::message::Reasoning::encrypted("opaque-cot-bytes").with_id("rs_enc".to_string());
        let message = Message::Assistant {
            id: Some("assistant_encrypted".to_string()),
            content: OneOrMany::many(vec![
                AssistantContent::Reasoning(encrypted),
                AssistantContent::text("answer"),
            ])?,
        };
        recorder
            .append(PersistedItem::HistoryMessage(HistoryMessage::Full {
                message: message.clone(),
            }))
            .await?;

        let state = load_resume_state(recorder.path()).await?;
        // rig 0.38's `Text::additional_params` is `#[serde(flatten)]`, so an empty
        // one round-trips through JSON as `Some({})` rather than `None`. That is
        // wire-identical, so compare serialized forms instead of Rust `PartialEq`.
        assert_eq!(
            serde_json::to_value(&state.history)?,
            serde_json::to_value(vec![message])?,
        );

        let _ = fs::remove_dir_all(&root).await;
        Ok(())
    }
}
