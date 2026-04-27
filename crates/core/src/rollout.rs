use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use rig::{
    OneOrMany,
    message::{AssistantContent, Message, Text, UserContent},
};
use serde::{Deserialize, Serialize};
use smooth_protocol::{EventMsg, ThreadId, TurnInterruptedEvent};
use time::{
    OffsetDateTime,
    format_description::FormatItem,
    format_description::well_known::Rfc3339,
    macros::format_description,
};
use tokio::{
    fs::{self, File},
    io::AsyncWriteExt,
    sync::Mutex,
};

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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub(crate) enum HistoryMessage {
    UserText { text: String },
    AssistantText { text: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SessionMeta {
    pub thread_id: ThreadId,
    pub cwd: PathBuf,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum PersistedItem {
    SessionMeta(SessionMeta),
    HistoryMessage(HistoryMessage),
    Event(EventMsg),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RolloutEnvelope {
    timestamp: String,
    item: PersistedItem,
}

impl RolloutRecorder {
    pub(crate) async fn create(workspace_root: &Path, thread_id: ThreadId, cwd: &Path) -> Result<Self> {
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

pub(crate) fn persist_event(event: &EventMsg) -> bool {
    matches!(
        event,
        EventMsg::SessionConfigured(_)
            | EventMsg::TurnStarted(_)
            | EventMsg::TurnCompleted(_)
            | EventMsg::TurnInterrupted(_)
            | EventMsg::AgentMessage(_)
            | EventMsg::AgentMessageCompleted(_)
            | EventMsg::ToolCallStarted(_)
            | EventMsg::ToolCallCompleted(_)
            | EventMsg::UserMessage(_)
            | EventMsg::Error(_)
    )
}

pub(crate) async fn load_resume_state(path: &Path) -> Result<ResumeState> {
    let contents = fs::read_to_string(path).await?;
    let mut meta: Option<SessionMeta> = None;
    let mut history = Vec::new();
    let mut initial_messages = Vec::new();
    let mut max_turn_index = None::<u64>;
    let mut has_open_turn = false;
    let mut has_terminal_turn = false;

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
            PersistedItem::HistoryMessage(HistoryMessage::UserText { text }) => {
                history.push(Message::User {
                    content: OneOrMany::one(UserContent::Text(Text { text })),
                });
            }
            PersistedItem::HistoryMessage(HistoryMessage::AssistantText { text }) => {
                history.push(Message::Assistant {
                    id: None,
                    content: OneOrMany::one(AssistantContent::text(text)),
                });
            }
            PersistedItem::Event(event) => {
                update_turn_tracking(&event, &mut max_turn_index, &mut has_open_turn, &mut has_terminal_turn);
                initial_messages.push(event);
            }
        }
    }

    let meta = meta.with_context(|| format!("missing session metadata in {}", path.display()))?;
    if has_open_turn && !has_terminal_turn {
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
    })
}

pub(crate) async fn list_threads(workspace_root: &Path) -> Result<Vec<ThreadSummary>> {
    let sessions_root = sessions_root(workspace_root);
    if !fs::try_exists(&sessions_root).await.unwrap_or(false) {
        return Ok(Vec::new());
    }

    let mut rollout_paths = Vec::new();
    collect_rollout_paths(&sessions_root, &mut rollout_paths).await?;

    let mut threads = Vec::new();
    for path in rollout_paths {
        if let Ok(summary) = summarize_rollout(&path).await {
            threads.push(summary);
        }
    }
    threads.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
    Ok(threads)
}

pub(crate) async fn find_thread_path(workspace_root: &Path, thread_id: ThreadId) -> Result<PathBuf> {
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
                *max_turn_index = Some(max_turn_index.map_or(turn_index, |current| current.max(turn_index)));
            }
            *has_open_turn = true;
            *has_terminal_turn = false;
        }
        EventMsg::TurnCompleted(_) | EventMsg::TurnInterrupted(_) => {
            *has_open_turn = false;
            *has_terminal_turn = true;
        }
        _ => {}
    }
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
            PersistedItem::HistoryMessage(HistoryMessage::UserText { text }) => {
                last_user_message = Some(text);
            }
            PersistedItem::HistoryMessage(HistoryMessage::AssistantText { text }) => {
                last_assistant_message = Some(text);
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
    workspace_root.join(".smooth-code").join("sessions")
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
    use smooth_protocol::{SessionConfiguredEvent, TurnStartedEvent};

    fn test_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("smooth-code-rollout-{name}-{}", ThreadId::new()))
    }

    #[test]
    fn create_and_list_rollout_threads() {
        runtime().block_on(async {
            let root = test_root("list");
            let cwd = root.join("workspace");
            fs::create_dir_all(&cwd).await.expect("create cwd");

            let thread_id = ThreadId::new();
            let recorder = RolloutRecorder::create(&root, thread_id, &cwd)
                .await
                .expect("create recorder");
            recorder
                .append(PersistedItem::HistoryMessage(HistoryMessage::UserText {
                    text: "hello".to_string(),
                }))
                .await
                .expect("append user");

            let threads = list_threads(&root).await.expect("list threads");
            assert_eq!(threads.len(), 1);
            assert_eq!(threads[0].thread_id, thread_id);
            assert_eq!(threads[0].last_user_message.as_deref(), Some("hello"));

            let _ = fs::remove_dir_all(&root).await;
        });
    }

    #[test]
    fn resume_state_reconstructs_history_and_recovery_interrupt() {
        runtime().block_on(async {
            let root = test_root("resume");
            let cwd = root.join("workspace");
            fs::create_dir_all(&cwd).await.expect("create cwd");

            let thread_id = ThreadId::new();
            let recorder = RolloutRecorder::create(&root, thread_id, &cwd)
                .await
                .expect("create recorder");
            recorder
                .append(PersistedItem::Event(EventMsg::SessionConfigured(
                    SessionConfiguredEvent {
                        thread_id: thread_id.to_string(),
                        rollout_path: Some(recorder.path().display().to_string()),
                    },
                )))
                .await
                .expect("append configured");
            recorder
                .append(PersistedItem::HistoryMessage(HistoryMessage::UserText {
                    text: "hello".to_string(),
                }))
                .await
                .expect("append user");
            recorder
                .append(PersistedItem::Event(EventMsg::TurnStarted(TurnStartedEvent {
                    thread_id: thread_id.to_string(),
                    turn_id: "4".to_string(),
                })))
                .await
                .expect("append turn started");

            let state = load_resume_state(recorder.path())
                .await
                .expect("load resume state");
            assert_eq!(state.thread_id, thread_id);
            assert_eq!(state.next_turn_index, 5);
            assert_eq!(state.history.len(), 1);
            assert!(matches!(
                state.initial_messages.last(),
                Some(EventMsg::TurnInterrupted(turn)) if turn.reason == "resume_recovery"
            ));

            let _ = fs::remove_dir_all(&root).await;
        });
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build runtime")
    }
}
