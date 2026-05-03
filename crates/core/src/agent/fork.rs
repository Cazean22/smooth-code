use rig::{
    OneOrMany,
    message::{AssistantContent, Message, Text, UserContent},
};

use crate::rollout::{HistoryMessage, PersistedItem};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SpawnAgentForkMode {
    ParentHistory,
}

pub(crate) fn keep_forked_rollout_item(item: &PersistedItem) -> bool {
    matches!(item, PersistedItem::HistoryMessage(_))
}

pub(crate) fn persisted_items_to_messages(
    items: impl IntoIterator<Item = PersistedItem>,
) -> Vec<Message> {
    items
        .into_iter()
        .filter(keep_forked_rollout_item)
        .filter_map(|item| match item {
            PersistedItem::HistoryMessage(HistoryMessage::UserText { text }) => {
                Some(Message::User {
                    content: OneOrMany::one(UserContent::Text(Text { text })),
                })
            }
            PersistedItem::HistoryMessage(HistoryMessage::AssistantText { text }) => {
                Some(Message::Assistant {
                    id: None,
                    content: OneOrMany::one(AssistantContent::text(text)),
                })
            }
            PersistedItem::SessionMeta(_) | PersistedItem::Event(_) => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use crate::rollout::{HistoryMessage, PersistedItem, SessionMeta};
    use smooth_protocol::{EventMsg, ThreadId};
    use std::path::PathBuf;

    use super::{keep_forked_rollout_item, persisted_items_to_messages};

    #[test]
    fn keeps_only_history_items() {
        assert!(keep_forked_rollout_item(&PersistedItem::HistoryMessage(
            HistoryMessage::UserText {
                text: "hello".to_string(),
            }
        )));
        assert!(!keep_forked_rollout_item(&PersistedItem::SessionMeta(
            SessionMeta {
                thread_id: ThreadId::new(),
                cwd: PathBuf::from("."),
                created_at: "now".to_string(),
            }
        )));
        assert!(!keep_forked_rollout_item(&PersistedItem::Event(
            EventMsg::AgentMessage("done".to_string(),)
        )));
    }

    #[test]
    fn converts_persisted_history_back_to_messages() {
        let messages = persisted_items_to_messages(vec![
            PersistedItem::SessionMeta(SessionMeta {
                thread_id: ThreadId::new(),
                cwd: PathBuf::from("."),
                created_at: "now".to_string(),
            }),
            PersistedItem::HistoryMessage(HistoryMessage::UserText {
                text: "hello".to_string(),
            }),
            PersistedItem::HistoryMessage(HistoryMessage::AssistantText {
                text: "world".to_string(),
            }),
        ]);

        assert_eq!(messages.len(), 2);
    }
}
