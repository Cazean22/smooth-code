use ratatui::text::Line;

pub(crate) struct MarkdownStreamCollector {
    buffer: String,
    committed_line_count: usize,
    last_parsed_newline_idx: Option<usize>,
    width: Option<usize>,
    /// Test-only counter incremented before each `append_markdown`
    /// call so tests can assert the optimization avoided a redundant
    /// re-parse. Counted in both `commit_complete_lines` and
    /// `finalize_and_drain` for symmetry, even though only the former
    /// is currently exercised by tests. Reset by `clear()`.
    #[cfg(test)]
    parse_count: usize,
}

impl MarkdownStreamCollector {
    pub(crate) fn new(width: Option<usize>) -> Self {
        Self {
            buffer: String::new(),
            committed_line_count: 0,
            last_parsed_newline_idx: None,
            width,
            #[cfg(test)]
            parse_count: 0,
        }
    }

    pub(crate) fn push_delta(&mut self, delta: &str) {
        self.buffer.push_str(delta);
    }

    pub(crate) fn pending_tail(&self) -> &str {
        let start = self.buffer.rfind('\n').map(|idx| idx + 1).unwrap_or(0);
        &self.buffer[start..]
    }

    pub(crate) fn commit_complete_lines(&mut self) -> Vec<Line<'static>> {
        let Some(last_newline_idx) = self.buffer.rfind('\n') else {
            return Vec::new();
        };

        if Some(last_newline_idx) == self.last_parsed_newline_idx {
            return Vec::new();
        }

        let mut rendered = Vec::new();
        #[cfg(test)]
        {
            self.parse_count += 1;
        }
        crate::markdown::append_markdown(
            &self.buffer[..=last_newline_idx],
            self.width,
            &mut rendered,
        );
        // Update the marker before the line-count early return below so
        // the optimization still fires when a newly advanced newline
        // produced zero rendered lines (e.g. `# h\n` then `\n`, where
        // Renderer::finish trims the heading's trailing blank). See
        // commit_skips_reparse_after_zero_diff_newline.
        self.last_parsed_newline_idx = Some(last_newline_idx);
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
        #[cfg(test)]
        {
            self.parse_count += 1;
        }
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
        self.last_parsed_newline_idx = None;
        #[cfg(test)]
        {
            self.parse_count = 0;
        }
    }

    #[cfg(test)]
    pub(crate) fn parse_count(&self) -> usize {
        self.parse_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_texts(lines: Vec<Line<'static>>) -> Vec<String> {
        lines
            .into_iter()
            .map(|line| {
                line.spans
                    .into_iter()
                    .map(|span| span.content.into_owned())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn no_commit_until_newline() {
        let mut collector = MarkdownStreamCollector::new(None);
        collector.push_delta("hello");
        assert!(collector.commit_complete_lines().is_empty());
        collector.push_delta("\n");
        assert_eq!(collector.commit_complete_lines().len(), 1);
    }

    #[test]
    fn commit_is_idempotent_without_new_newline() {
        let mut collector = MarkdownStreamCollector::new(None);

        collector.push_delta("hello\n");
        assert_eq!(line_texts(collector.commit_complete_lines()), vec!["hello"]);
        assert_eq!(collector.parse_count(), 1);

        collector.push_delta("more");
        assert!(collector.commit_complete_lines().is_empty());
        assert_eq!(collector.parse_count(), 1);

        collector.push_delta("\n");
        assert_eq!(line_texts(collector.commit_complete_lines()), vec!["more"]);
        assert_eq!(collector.parse_count(), 2);
    }

    #[test]
    fn commit_skips_reparse_after_zero_diff_newline() {
        let mut collector = MarkdownStreamCollector::new(None);

        collector.push_delta("# h\n");
        assert_eq!(line_texts(collector.commit_complete_lines()), vec!["h"]);
        assert_eq!(collector.parse_count(), 1);

        collector.push_delta("\n");
        assert!(collector.commit_complete_lines().is_empty());
        assert_eq!(collector.parse_count(), 2);

        collector.push_delta("tail");
        assert!(collector.commit_complete_lines().is_empty());
        assert_eq!(collector.parse_count(), 2);

        collector.push_delta("\n");
        assert_eq!(
            line_texts(collector.commit_complete_lines()),
            vec![String::new(), "tail".to_owned()]
        );
        assert_eq!(collector.parse_count(), 3);
    }

    #[test]
    fn clear_resets_parsed_newline_marker() {
        let mut collector = MarkdownStreamCollector::new(None);

        collector.push_delta("a\n");
        assert_eq!(line_texts(collector.commit_complete_lines()), vec!["a"]);
        assert_eq!(collector.parse_count(), 1);

        collector.clear();
        assert_eq!(collector.parse_count(), 0);

        collector.push_delta("a\n");
        assert_eq!(line_texts(collector.commit_complete_lines()), vec!["a"]);
        assert_eq!(collector.parse_count(), 1);
    }
}
