use rig::message::Message;

#[derive(Debug, Clone, Default)]
pub(crate) struct ContextManager {
    /// The oldest items are at the beginning of the vector.
    items: Vec<Message>,
}

impl ContextManager {
    pub(crate) fn items(&self) -> &[Message] {
        &self.items
    }

    pub(crate) fn replace(&mut self, items: Vec<Message>) {
        self.items = items;
    }
}
