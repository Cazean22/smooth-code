use rig::message::Message;

#[derive(Debug, Clone, Default)]
pub(crate) struct ContextManager {
    /// The oldest items are at the beginning of the vector.
    items: Vec<Message>,
    /// Bumped whenever history is rewritten, such as compaction or rollback.
    history_version: u64,
    token_info: Option<String>,
    /// Reference context snapshot used for diffing and producing model-visible
    /// settings update items.
    ///
    /// This is the baseline for the next regular model turn, and may already
    /// match the current turn after context updates are persisted.
    ///
    /// When this is `None`, settings diffing treats the next turn as having no
    /// baseline and emits a full reinjection of context state. Rollback may
    /// also clear this when it trims a mixed initial-context developer bundle
    /// whose non-diff fragments no longer exist in the surviving history.
    reference_context_item: Option<Message>,
}

impl ContextManager {
    pub(crate) fn items(&self) -> &[Message] {
        &self.items
    }

    pub(crate) fn replace(&mut self, items: Vec<Message>) {
        self.items = items;
        self.history_version = self.history_version.saturating_add(1);
    }

    pub(crate) fn push(&mut self, item: Message) {
        self.items.push(item);
        self.history_version = self.history_version.saturating_add(1);
    }
}
