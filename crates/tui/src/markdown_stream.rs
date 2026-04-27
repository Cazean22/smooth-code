use ratatui::text::Line;

pub(crate) struct MarkdownStreamCollector {
    buffer: String,
    committed_line_count: usize,
    width: Option<usize>,
}

impl MarkdownStreamCollector {
    pub(crate) fn new(width: Option<usize>) -> Self {
        Self {
            buffer: String::new(),
            committed_line_count: 0,
            width,
        }
    }

    pub(crate) fn push_delta(&mut self, delta: &str) {
        self.buffer.push_str(delta);
    }

    pub(crate) fn commit_complete_lines(&mut self) -> Vec<Line<'static>> {
        let Some(last_newline_idx) = self.buffer.rfind('\n') else {
            return Vec::new();
        };

        let mut rendered = Vec::new();
        crate::markdown::append_markdown(
            &self.buffer[..=last_newline_idx],
            self.width,
            &mut rendered,
        );
        if self.committed_line_count >= rendered.len() {
            return Vec::new();
        }

        let out = rendered[self.committed_line_count..].to_vec();
        self.committed_line_count = rendered.len();
        out
    }

    pub(crate) fn finalize_and_drain(&mut self) -> Vec<Line<'static>> {
        let mut source = self.buffer.clone();
        if !source.ends_with('\n') {
            source.push('\n');
        }

        let mut rendered = Vec::new();
        crate::markdown::append_markdown(&source, self.width, &mut rendered);
        let out = if self.committed_line_count >= rendered.len() {
            Vec::new()
        } else {
            rendered[self.committed_line_count..].to_vec()
        };
        self.clear();
        out
    }

    pub(crate) fn clear(&mut self) {
        self.buffer.clear();
        self.committed_line_count = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_commit_until_newline() {
        let mut collector = MarkdownStreamCollector::new(None);
        collector.push_delta("hello");
        assert!(collector.commit_complete_lines().is_empty());
        collector.push_delta("\n");
        assert_eq!(collector.commit_complete_lines().len(), 1);
    }
}
