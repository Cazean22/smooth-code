//! Model-facing encoding of subagent results.
//!
//! A `spawn_agent` result reaches the model as JSON (`SpawnAgentResult`). This
//! encoding lives here, separate from the turn loop, because two callers need
//! it: the turn loop (`tasks::regular`) encodes live status and inline
//! completions, and resume (`rollout`) reconstructs the same model-facing user
//! message from a persisted typed [`CompletionEntry`]. Routing both through one
//! encoder keeps the reconstructed JSON byte-identical to what the live turn
//! produced.

use rig::{
    OneOrMany,
    message::{Message, Text, UserContent},
};
use serde::{Deserialize, Serialize};
use smooth_protocol::{AgentPath, AgentStatus, ErrorInfo, ThreadId};

use crate::agent::{
    control::InlineChildCompletion, registry::AgentMetadata, status::last_assistant_message,
};

/// The model-facing JSON shape for a `spawn_agent` result. The system prompt
/// instructs the model on these fields, so the serialized shape is a contract:
/// do not change field names/values without updating the prompt and tests.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SpawnAgentResult {
    event: String,
    thread_id: String,
    agent_path: String,
    agent_nickname: Option<String>,
    status: Option<String>,
    #[serde(default)]
    status_detail: Option<AgentStatus>,
    #[serde(default)]
    last_assistant_message: Option<String>,
    next_action: String,
    instructions: String,
}

/// A finished subagent's result, captured in typed form. It is both rendered
/// into the model-facing user message during the turn and persisted as the
/// durable record (`rollout::HistoryMessage::SubagentCompletion`); on resume the
/// same entry reconstructs a byte-identical model-facing message.
///
/// `last_assistant_message` stores the inline-wait *override* (not the value
/// resolved against `status`); [`Self::to_model_json`] re-applies the
/// `.or_else(|| last_assistant_message(status))` fallback, reproducing exactly
/// what the live turn encoded.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CompletionEntry {
    pub(crate) child_thread_id: Option<ThreadId>,
    pub(crate) agent_path: AgentPath,
    pub(crate) agent_nickname: Option<String>,
    pub(crate) status: AgentStatus,
    #[serde(default)]
    pub(crate) last_assistant_message: Option<String>,
}

impl CompletionEntry {
    /// Build an entry from a resolved inline-wait result, mapping a waiter
    /// failure to an `Errored` status exactly as the turn loop did inline.
    pub(crate) fn from_inline(
        metadata: &AgentMetadata,
        completion: Result<InlineChildCompletion, String>,
    ) -> Self {
        let (status, last_assistant_message) = match completion {
            Ok(completion) => (completion.status, completion.last_assistant_message),
            Err(message) => (
                AgentStatus::Errored(
                    ErrorInfo::new("spawn_agent_inline_wait_failed", message)
                        .with_source("smooth-core"),
                ),
                None,
            ),
        };
        Self {
            child_thread_id: metadata.agent_id,
            agent_path: metadata.agent_path.clone(),
            agent_nickname: metadata.agent_nickname.clone(),
            status,
            last_assistant_message,
        }
    }

    /// The model-facing `agent_completed` JSON for this completion. Falls back
    /// to the error string on the (practically unreachable) encode failure, the
    /// same contract the inline path used.
    pub(crate) fn to_model_json(&self) -> String {
        encode_spawn_agent_result_value(&spawn_agent_result(
            self.child_thread_id,
            &self.agent_path,
            self.agent_nickname.as_deref(),
            &self.status,
            self.last_assistant_message.clone(),
        ))
        .unwrap_or_else(|message| message)
    }
}

/// Reconstruct the single grouped user message the model sees for a batch of
/// completions. Mirrors `tasks::regular::text_items_to_user_message`: one
/// `Text` content item per completion in one `Message::User`.
pub(crate) fn completion_entries_to_user_message(entries: &[CompletionEntry]) -> Option<Message> {
    let content = entries
        .iter()
        .map(|entry| {
            UserContent::Text(Text {
                text: entry.to_model_json(),
            })
        })
        .collect::<Vec<_>>();
    OneOrMany::many(content)
        .ok()
        .map(|content| Message::User { content })
}

pub(crate) fn encode_spawn_agent_result(
    metadata: &AgentMetadata,
    status: &AgentStatus,
    last_assistant_message_override: Option<String>,
) -> Result<String, String> {
    encode_spawn_agent_result_value(&spawn_agent_result(
        metadata.agent_id,
        &metadata.agent_path,
        metadata.agent_nickname.as_deref(),
        status,
        last_assistant_message_override,
    ))
}

fn encode_spawn_agent_result_value(result: &SpawnAgentResult) -> Result<String, String> {
    serde_json::to_string(result)
        .map_err(|err| format!("failed to encode spawn_agent output: {err}"))
}

fn spawn_agent_result(
    agent_id: Option<ThreadId>,
    agent_path: &AgentPath,
    agent_nickname: Option<&str>,
    status: &AgentStatus,
    last_assistant_message_override: Option<String>,
) -> SpawnAgentResult {
    let is_live = matches!(status, AgentStatus::PendingInit | AgentStatus::Running);
    SpawnAgentResult {
        event: if is_live {
            String::from("agent_status")
        } else {
            String::from("agent_completed")
        },
        thread_id: agent_id
            .map(|thread_id| thread_id.to_string())
            .unwrap_or_default(),
        agent_path: agent_path.to_string(),
        agent_nickname: agent_nickname.map(|nickname| nickname.to_string()),
        status: Some(agent_status_label(status).to_string()),
        status_detail: Some(status.clone()),
        last_assistant_message: last_assistant_message_override
            .or_else(|| last_assistant_message(status)),
        next_action: if is_live {
            String::from("wait_for_agent_completed")
        } else {
            String::from("use_agent_result")
        },
        instructions: if is_live {
            String::from(
                "This sub-agent is still running. Do not answer or guess from this status. No wait tool is needed; wait for a later user message with event=\"agent_completed\" and the same thread_id.",
            )
        } else {
            String::from(
                "This sub-agent has finished. Use last_assistant_message and status_detail as the sub-agent result.",
            )
        },
    }
}

fn agent_status_label(status: &AgentStatus) -> &'static str {
    match status {
        AgentStatus::PendingInit => "pending_init",
        AgentStatus::Running => "running",
        AgentStatus::Interrupted => "interrupted",
        AgentStatus::Completed(_) => "completed",
        AgentStatus::Errored(_) => "errored",
        AgentStatus::Shutdown => "shutdown",
        AgentStatus::NotFound => "not_found",
    }
}
