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
        let mut lines = self.committed_lines.clone();
        let tail = self.collector.pending_tail();
        if !tail.is_empty() {
            lines.push(Line::raw(tail.to_owned()));
        }
        (!lines.is_empty()).then_some(lines)
    }

    pub(crate) fn finalize_lines(mut self) -> Option<Vec<Line<'static>>> {
        let remaining = self.collector.finalize_and_drain();
        self.committed_lines.extend(remaining);
        (!self.committed_lines.is_empty()).then_some(self.committed_lines)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines_to_strings(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn snapshot_includes_pending_tail_before_newline() -> Result<(), Box<dyn std::error::Error>> {
        let mut controller = StreamController::new(None);
        let _ = controller.push("hello");

        let lines = controller
            .snapshot_lines()
            .ok_or_else(|| std::io::Error::other("pending tail should render"))?;
        assert_eq!(lines_to_strings(&lines), vec![String::from("hello")]);
        Ok(())
    }

    #[test]
    fn snapshot_replaces_pending_tail_after_newline_commit()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut controller = StreamController::new(None);
        let _ = controller.push("hello");
        let _ = controller.push(" world\n");

        let lines = controller
            .snapshot_lines()
            .ok_or_else(|| std::io::Error::other("committed markdown line should render"))?;
        assert_eq!(lines_to_strings(&lines), vec![String::from("hello world")]);
        Ok(())
    }

    #[test]
    fn snapshot_includes_committed_lines_and_pending_tail() -> Result<(), Box<dyn std::error::Error>>
    {
        let mut controller = StreamController::new(None);
        let _ = controller.push("hello\nwor");

        let lines = controller.snapshot_lines().ok_or_else(|| {
            std::io::Error::other("committed line and pending tail should render")
        })?;
        assert_eq!(
            lines_to_strings(&lines),
            vec![String::from("hello"), String::from("wor")]
        );
        Ok(())
    }
}
