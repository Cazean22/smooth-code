use std::collections::HashMap;
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
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
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
        self.append_inner(item, false).await
    }

    /// Like [`append`](Self::append), but `fsync`s the file after writing so the
    /// record survives an OS crash / power loss, not just a process exit.
    /// Reserved for durable turn results and terminal lifecycle events, where a
    /// lost tail would make a completed turn resume as interrupted.
    pub(crate) async fn append_synced(&self, item: PersistedItem) -> Result<()> {
        self.append_inner(item, true).await
    }

    async fn append_inner(&self, item: PersistedItem, sync: bool) -> Result<()> {
        let envelope = RolloutEnvelope {
            timestamp: now_rfc3339()?,
            item,
        };
        let mut line = serde_json::to_vec(&envelope)?;
        line.push(b'\n');
        let mut file = self.file.lock().await;
        file.write_all(&line).await?;
        file.flush().await?;
        if sync {
            file.sync_data().await?;
        }
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

/// Whether an event terminates a turn (completion, interruption, or error). Such
/// records are `fsync`ed when persisted: a terminal record lost to an OS crash
/// would make a finished turn resume as still-open / interrupted.
pub(crate) fn is_terminal_lifecycle_event(event: &EventMsg) -> bool {
    matches!(
        event,
        EventMsg::TurnCompleted(_) | EventMsg::TurnInterrupted(_) | EventMsg::Error(_)
    )
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
    let file = File::open(path).await?;
    let mut lines = BufReader::new(file).lines();
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
    let mut plan_mode = false;

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let envelope = match serde_json::from_str::<RolloutEnvelope>(&line) {
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
                        // Truncate the errored turn's partial tail immediately, not
                        // via a deferred marker: a failed-persistence tail (e.g. an
                        // assistant tool call whose matching result never wrote) is
                        // permanently malformed, so a *later* completed turn must not
                        // be able to resurrect it. Doing it inline means subsequent
                        // turns append onto the already-truncated history.
                        if recovery == RecoveryMode::Resume {
                            history.truncate(start);
                            interrupted_synthetic_start = None;
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
    if recovery == RecoveryMode::Resume && has_open_turn && !has_terminal_turn {
        // A turn that started but never reached a terminal event (process crashed
        // mid-turn): drop its partial tail and surface it as interrupted. (Errored
        // turns are already truncated inline above.)
        if let Some(start) = open_turn_history_start {
            history.truncate(start);
        }
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
    let rollout_paths = collect_workspace_rollout_paths(workspace_root).await?;
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
    let rollout_paths = collect_workspace_rollout_paths(workspace_root).await?;
    rollout_paths
        .into_iter()
        .find(|path| rollout_thread_id(path) == Some(thread_id))
        .with_context(|| format!("unknown thread id: {thread_id}"))
}

/// Build the `thread_id`→rollout-path map for an entire workspace in a single
/// directory walk, reading no file contents. Resume uses this to resolve a whole
/// child subtree without re-walking the sessions tree per child.
pub(crate) async fn collect_rollout_path_map(
    workspace_root: &Path,
) -> Result<HashMap<ThreadId, PathBuf>> {
    let rollout_paths = collect_workspace_rollout_paths(workspace_root).await?;
    let mut map = HashMap::with_capacity(rollout_paths.len());
    for path in rollout_paths {
        if let Some(thread_id) = rollout_thread_id(&path) {
            map.insert(thread_id, path);
        }
    }
    Ok(map)
}

async fn collect_workspace_rollout_paths(workspace_root: &Path) -> Result<Vec<PathBuf>> {
    let mut rollout_paths = Vec::new();
    let sessions_root = sessions_root(workspace_root);
    if fs::try_exists(&sessions_root).await.unwrap_or(false) {
        collect_rollout_paths(&sessions_root, &mut rollout_paths).await?;
    }
    Ok(rollout_paths)
}

/// Extract the thread id a rollout filename encodes. [`create_rollout_path`]
/// writes `rollout-<timestamp>-<thread_id>.jsonl`, and `ThreadId` is a fixed
/// 36-char UUID, so the id is the trailing 36 chars of the `rollout-`-prefixed
/// stem. Returns `None` for names that don't match the template or don't parse
/// as a thread id (e.g. unrelated `.jsonl` files).
fn rollout_thread_id(path: &Path) -> Option<ThreadId> {
    let stem = path.file_stem()?.to_str()?;
    let rest = stem.strip_prefix("rollout-")?;
    // ASCII throughout (timestamp + hyphens + hex UUID), so byte-slicing the
    // trailing 36 chars lands on a char boundary.
    let candidate = rest.get(rest.len().checked_sub(36)?..)?;
    candidate.parse::<ThreadId>().ok()
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
    let file = File::open(path).await?;
    let mut lines = BufReader::new(file).lines();
    let mut meta: Option<SessionMeta> = None;
    let mut updated_at = None::<String>;
    let mut last_user_message = None;
    let mut last_assistant_message = None;

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let envelope = match serde_json::from_str::<RolloutEnvelope>(&line) {
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

    #[tokio::test]
    async fn find_thread_path_resolves_by_filename_without_reading_contents() -> Result<()> {
        let root = test_root("find-by-filename");
        let cwd = root.join("workspace");
        fs::create_dir_all(&cwd).await?;

        // A real rollout (with content).
        let real_id = ThreadId::new();
        let recorder = RolloutRecorder::create(&root, real_id, &cwd).await?;

        // A content-less rollout whose id lives only in the filename: resolution
        // must succeed without parsing the (empty) body.
        let empty_id = ThreadId::new();
        let sessions = sessions_root(&root);
        fs::create_dir_all(&sessions).await?;
        let empty_path = sessions.join(format!("rollout-2026-01-01T00-00-00-{empty_id}.jsonl"));
        fs::write(&empty_path, b"").await?;

        assert_eq!(
            find_thread_path(&root, real_id).await?.as_path(),
            recorder.path()
        );
        assert_eq!(find_thread_path(&root, empty_id).await?, empty_path);
        assert!(find_thread_path(&root, ThreadId::new()).await.is_err());

        let map = collect_rollout_path_map(&root).await?;
        assert_eq!(
            map.get(&real_id).map(PathBuf::as_path),
            Some(recorder.path())
        );
        assert_eq!(map.get(&empty_id), Some(&empty_path));

        let _ = fs::remove_dir_all(&root).await;
        Ok(())
    }

    #[tokio::test]
    async fn resume_truncates_partial_tail_of_crashed_open_turn_without_terminal_event()
    -> Result<()> {
        let root = test_root("resume-crashed-open-turn");
        let cwd = root.join("workspace");
        fs::create_dir_all(&cwd).await?;

        let thread_id = ThreadId::new();
        let recorder = RolloutRecorder::create(&root, thread_id, &cwd).await?;
        // A completed stable turn's model history.
        recorder
            .append(PersistedItem::HistoryMessage(HistoryMessage::Full {
                message: user_history_message("stable prompt".to_string()),
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
        // A turn that started, persisted a partial tail, then crashed — no
        // `TurnCompleted` / `TurnInterrupted` / `Error` ever followed.
        recorder
            .append(PersistedItem::Event(EventMsg::TurnStarted(
                TurnStartedEvent {
                    thread_id: thread_id.to_string(),
                    turn_id: "5".to_string(),
                },
            )))
            .await?;
        recorder
            .append(PersistedItem::HistoryMessage(HistoryMessage::Full {
                message: user_history_message("crashed prompt".to_string()),
            }))
            .await?;
        recorder
            .append(PersistedItem::HistoryMessage(HistoryMessage::Full {
                message: Message::Assistant {
                    id: None,
                    content: OneOrMany::one(AssistantContent::text("partial answer".to_string())),
                },
            }))
            .await?;

        let state = load_resume_state(recorder.path()).await?;
        // The crashed turn's partial tail is dropped back to the open-turn start.
        assert_eq!(state.history.len(), 2);
        assert_eq!(
            user_message_text(&state.history[0]).as_deref(),
            Some("stable prompt")
        );
        assert_eq!(
            assistant_message_text(&state.history[1]).as_deref(),
            Some("stable answer")
        );
        assert_eq!(state.next_turn_index, 6);
        // A crashed open turn surfaces as interrupted on resume.
        assert!(matches!(
            state.initial_messages.last(),
            Some(EventMsg::TurnInterrupted(turn)) if turn.reason == "resume_recovery"
        ));

        let _ = fs::remove_dir_all(&root).await;
        Ok(())
    }

    #[tokio::test]
    async fn resume_keeps_history_when_an_errored_turn_is_followed_by_a_completed_turn()
    -> Result<()> {
        let root = test_root("resume-error-then-complete");
        let cwd = root.join("workspace");
        fs::create_dir_all(&cwd).await?;

        let thread_id = ThreadId::new();
        let recorder = RolloutRecorder::create(&root, thread_id, &cwd).await?;
        recorder
            .append(PersistedItem::HistoryMessage(HistoryMessage::Full {
                message: user_history_message("stable prompt".to_string()),
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
        // A turn that errored before any assistant output (no persisted tail, as
        // the live path no longer eagerly writes the failed prompt).
        recorder
            .append(PersistedItem::Event(EventMsg::TurnStarted(
                TurnStartedEvent {
                    thread_id: thread_id.to_string(),
                    turn_id: "7".to_string(),
                },
            )))
            .await?;
        recorder
            .append(PersistedItem::Event(EventMsg::Error(ErrorEvent {
                error: ErrorInfo::new("turn_failed", "provider reset"),
            })))
            .await?;
        // A later turn that completed cleanly: history through it must survive,
        // and the earlier error's `unstable_history_start` must be cleared.
        recorder
            .append(PersistedItem::Event(EventMsg::TurnStarted(
                TurnStartedEvent {
                    thread_id: thread_id.to_string(),
                    turn_id: "8".to_string(),
                },
            )))
            .await?;
        recorder
            .append(PersistedItem::HistoryMessage(HistoryMessage::Full {
                message: user_history_message("recovered prompt".to_string()),
            }))
            .await?;
        recorder
            .append(PersistedItem::HistoryMessage(HistoryMessage::Full {
                message: Message::Assistant {
                    id: None,
                    content: OneOrMany::one(AssistantContent::text("recovered answer".to_string())),
                },
            }))
            .await?;
        recorder
            .append(PersistedItem::Event(EventMsg::TurnCompleted(
                cazean_protocol::TurnCompletedEvent {
                    thread_id: thread_id.to_string(),
                    turn_id: "8".to_string(),
                    last_assistant_message: Some("recovered answer".to_string()),
                },
            )))
            .await?;

        let state = load_resume_state(recorder.path()).await?;
        assert_eq!(state.history.len(), 4);
        assert_eq!(
            user_message_text(&state.history[0]).as_deref(),
            Some("stable prompt")
        );
        assert_eq!(
            user_message_text(&state.history[2]).as_deref(),
            Some("recovered prompt")
        );
        assert_eq!(
            assistant_message_text(&state.history[3]).as_deref(),
            Some("recovered answer")
        );
        assert_eq!(state.next_turn_index, 9);
        // The completed turn means no crash-recovery interrupt is synthesized, and
        // the earlier error is still visible in the replayed transcript.
        assert!(!state.initial_messages.iter().any(|event| {
            matches!(event, EventMsg::TurnInterrupted(turn) if turn.reason == "resume_recovery")
        }));
        assert!(
            state
                .initial_messages
                .iter()
                .any(|event| matches!(event, EventMsg::Error(_)))
        );

        let _ = fs::remove_dir_all(&root).await;
        Ok(())
    }

    #[tokio::test]
    async fn resume_prunes_failed_persist_tail_even_when_a_later_turn_completes() -> Result<()> {
        // Models a turn whose persist partially succeeded (its prompt was written
        // but the assistant/tool-result tail was not) and then failed with `Error`
        // and no `TurnCompleted`, after which the user continued in the same live
        // session and a later turn completed. The earlier malformed tail must NOT
        // be resurrected by the later completion (the bug: a deferred unstable
        // marker cleared by any later `TurnCompleted`).
        let root = test_root("resume-failed-persist-then-complete");
        let cwd = root.join("workspace");
        fs::create_dir_all(&cwd).await?;

        let thread_id = ThreadId::new();
        let recorder = RolloutRecorder::create(&root, thread_id, &cwd).await?;
        recorder
            .append(PersistedItem::HistoryMessage(HistoryMessage::Full {
                message: user_history_message("stable prompt".to_string()),
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
        // The failed turn persisted a partial tail (just its prompt) before the
        // append/sync failure surfaced as `Error`.
        recorder
            .append(PersistedItem::HistoryMessage(HistoryMessage::Full {
                message: user_history_message("failed prompt".to_string()),
            }))
            .await?;
        recorder
            .append(PersistedItem::Event(EventMsg::Error(ErrorEvent {
                error: ErrorInfo::new("turn_failed", "rollout write failed"),
            })))
            .await?;
        // The user continued; a later turn completed cleanly.
        recorder
            .append(PersistedItem::Event(EventMsg::TurnStarted(
                TurnStartedEvent {
                    thread_id: thread_id.to_string(),
                    turn_id: "8".to_string(),
                },
            )))
            .await?;
        recorder
            .append(PersistedItem::HistoryMessage(HistoryMessage::Full {
                message: user_history_message("recovered prompt".to_string()),
            }))
            .await?;
        recorder
            .append(PersistedItem::HistoryMessage(HistoryMessage::Full {
                message: Message::Assistant {
                    id: None,
                    content: OneOrMany::one(AssistantContent::text("recovered answer".to_string())),
                },
            }))
            .await?;
        recorder
            .append(PersistedItem::Event(EventMsg::TurnCompleted(
                cazean_protocol::TurnCompletedEvent {
                    thread_id: thread_id.to_string(),
                    turn_id: "8".to_string(),
                    last_assistant_message: Some("recovered answer".to_string()),
                },
            )))
            .await?;

        let state = load_resume_state(recorder.path()).await?;
        // "failed prompt" is gone; the later completion did not resurrect it.
        assert_eq!(state.history.len(), 4);
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
            Some("recovered prompt")
        );
        assert_eq!(
            assistant_message_text(&state.history[3]).as_deref(),
            Some("recovered answer")
        );
        assert!(
            !state
                .history
                .iter()
                .any(|message| user_message_text(message).as_deref() == Some("failed prompt")),
            "the failed turn's partial tail must not survive into model history"
        );
        assert_eq!(state.next_turn_index, 9);

        let _ = fs::remove_dir_all(&root).await;
        Ok(())
    }
}
