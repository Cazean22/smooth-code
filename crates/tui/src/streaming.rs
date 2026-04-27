use ratatui::text::Line;

use crate::{history_cell::AgentMessageCell, markdown_stream::MarkdownStreamCollector};

pub(crate) struct StreamController {
    collector: MarkdownStreamCollector,
    committed_lines: Vec<Line<'static>>,
}

impl StreamController {
    pub(crate) fn new(width: Option<usize>) -> Self {
        Self {
            collector: MarkdownStreamCollector::new(width),
            committed_lines: Vec::new(),
        }
    }

    pub(crate) fn push(&mut self, delta: &str) -> bool {
        self.collector.push_delta(delta);
        let new_lines = self.collector.commit_complete_lines();
        if new_lines.is_empty() {
            return false;
        }
        self.committed_lines.extend(new_lines);
        true
    }

    pub(crate) fn snapshot_cell(&self) -> Option<AgentMessageCell> {
        (!self.committed_lines.is_empty())
            .then(|| AgentMessageCell::new(self.committed_lines.clone(), true))
    }

    pub(crate) fn finalize(mut self) -> Option<AgentMessageCell> {
        let remaining = self.collector.finalize_and_drain();
        self.committed_lines.extend(remaining);
        (!self.committed_lines.is_empty())
            .then(|| AgentMessageCell::new(self.committed_lines, true))
    }
}
