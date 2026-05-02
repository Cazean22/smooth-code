use ratatui::text::Line;

use crate::markdown_stream::MarkdownStreamCollector;

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

    pub(crate) fn snapshot_lines(&self) -> Option<Vec<Line<'static>>> {
        (!self.committed_lines.is_empty()).then(|| self.committed_lines.clone())
    }

    pub(crate) fn finalize_lines(mut self) -> Option<Vec<Line<'static>>> {
        let remaining = self.collector.finalize_and_drain();
        self.committed_lines.extend(remaining);
        (!self.committed_lines.is_empty()).then_some(self.committed_lines)
    }
}
