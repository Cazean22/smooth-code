use unicode_width::UnicodeWidthChar;

/// Cursor-aware single/multi-line text editor state, shared by the main
/// composer and the question picker's "Other" field.
#[derive(Debug, Default)]
pub(crate) struct ComposerState {
    text: String,
    cursor: usize,
    visual_goal_col: Option<usize>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ComposerVisualRow {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) width: usize,
}

impl ComposerState {
    pub(crate) fn as_str(&self) -> &str {
        &self.text
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub(crate) fn cursor(&self) -> usize {
        self.cursor
    }

    pub(crate) fn set_text(&mut self, text: String) {
        self.cursor = text.len();
        self.text = text;
        self.visual_goal_col = None;
    }

    pub(crate) fn take_text(&mut self) -> String {
        self.cursor = 0;
        self.visual_goal_col = None;
        std::mem::take(&mut self.text)
    }

    pub(crate) fn insert_char(&mut self, ch: char) {
        self.text.insert(self.cursor, ch);
        self.cursor += ch.len_utf8();
        self.visual_goal_col = None;
    }

    pub(crate) fn insert_str(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.text.insert_str(self.cursor, text);
        self.cursor += text.len();
        self.visual_goal_col = None;
    }

    pub(crate) fn insert_paste(&mut self, text: &str) {
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        self.insert_str(&normalized);
    }

    pub(crate) fn backspace(&mut self) {
        let Some(prev) = self.prev_cursor_boundary() else {
            return;
        };
        self.text.drain(prev..self.cursor);
        self.cursor = prev;
        self.visual_goal_col = None;
    }

    pub(crate) fn delete(&mut self) {
        let Some(next) = self.next_cursor_boundary() else {
            return;
        };
        self.text.drain(self.cursor..next);
        self.visual_goal_col = None;
    }

    pub(crate) fn move_left(&mut self) {
        if let Some(prev) = self.prev_cursor_boundary() {
            self.cursor = prev;
        }
        self.visual_goal_col = None;
    }

    pub(crate) fn move_right(&mut self) {
        if let Some(next) = self.next_cursor_boundary() {
            self.cursor = next;
        }
        self.visual_goal_col = None;
    }

    pub(crate) fn move_line_start(&mut self) {
        self.cursor = self.current_line_start();
        self.visual_goal_col = None;
    }

    pub(crate) fn move_line_end(&mut self) {
        self.cursor = self.current_line_end();
        self.visual_goal_col = None;
    }

    pub(crate) fn move_visual_up(&mut self, width: usize) {
        let (row, col) = self.cursor_visual_position(width);
        let goal_col = self.visual_goal_col.unwrap_or(col);
        if row > 0 {
            self.cursor = self.byte_offset_for_visual_position(row - 1, goal_col, width);
        }
        self.visual_goal_col = Some(goal_col);
    }

    pub(crate) fn move_visual_down(&mut self, width: usize) {
        let rows = self.visual_rows(width);
        let (row, col) = self.cursor_visual_position_in_rows(&rows);
        let goal_col = self.visual_goal_col.unwrap_or(col);
        if row + 1 < rows.len() {
            self.cursor = self.byte_offset_for_visual_position_in_rows(&rows, row + 1, goal_col);
        }
        self.visual_goal_col = Some(goal_col);
    }

    pub(crate) fn visual_rows(&self, width: usize) -> Vec<ComposerVisualRow> {
        let width = width.max(1);
        let mut rows = Vec::new();
        let mut row_start = 0;
        let mut row_width = 0usize;

        for (idx, ch) in self.text.char_indices() {
            if ch == '\n' {
                rows.push(ComposerVisualRow {
                    start: row_start,
                    end: idx,
                    width: row_width,
                });
                row_start = idx + ch.len_utf8();
                row_width = 0;
                continue;
            }

            let ch_width = Self::char_width(ch);
            if row_width > 0 && row_width.saturating_add(ch_width) > width {
                rows.push(ComposerVisualRow {
                    start: row_start,
                    end: idx,
                    width: row_width,
                });
                row_start = idx;
                row_width = 0;
            }
            row_width = row_width.saturating_add(ch_width);
        }

        rows.push(ComposerVisualRow {
            start: row_start,
            end: self.text.len(),
            width: row_width,
        });

        if !self.text.is_empty() && !self.text.ends_with('\n') && row_width >= width {
            rows.push(ComposerVisualRow {
                start: self.text.len(),
                end: self.text.len(),
                width: 0,
            });
        }

        rows
    }

    fn cursor_visual_position(&self, width: usize) -> (usize, usize) {
        let rows = self.visual_rows(width);
        self.cursor_visual_position_in_rows(&rows)
    }

    pub(crate) fn cursor_visual_position_in_rows(
        &self,
        rows: &[ComposerVisualRow],
    ) -> (usize, usize) {
        for (idx, row) in rows.iter().enumerate() {
            let next_starts_here = rows
                .get(idx + 1)
                .is_some_and(|next| next.start == self.cursor);
            let cursor_is_on_row = self.cursor >= row.start
                && (self.cursor < row.end
                    || (self.cursor == row.end
                        && !next_starts_here
                        && (self.cursor == self.text.len()
                            || self.text[self.cursor..].starts_with('\n')
                            || row.start == row.end)));
            if cursor_is_on_row {
                return (idx, self.display_width(row.start, self.cursor));
            }
        }

        let fallback_row = rows.len().saturating_sub(1);
        (fallback_row, rows.last().map(|row| row.width).unwrap_or(0))
    }

    fn byte_offset_for_visual_position(
        &self,
        row: usize,
        target_col: usize,
        width: usize,
    ) -> usize {
        let rows = self.visual_rows(width);
        self.byte_offset_for_visual_position_in_rows(&rows, row, target_col)
    }

    fn byte_offset_for_visual_position_in_rows(
        &self,
        rows: &[ComposerVisualRow],
        row: usize,
        target_col: usize,
    ) -> usize {
        let Some(row) = rows.get(row) else {
            return self.text.len();
        };
        let mut cursor = row.start;
        let mut col = 0usize;
        for (idx, ch) in self.text[row.start..row.end].char_indices() {
            let ch_width = Self::char_width(ch);
            if col.saturating_add(ch_width) > target_col {
                break;
            }
            col = col.saturating_add(ch_width);
            cursor = row.start + idx + ch.len_utf8();
        }
        cursor
    }

    fn prev_cursor_boundary(&self) -> Option<usize> {
        if self.cursor == 0 {
            return None;
        }
        self.text[..self.cursor]
            .char_indices()
            .last()
            .map(|(idx, _)| idx)
    }

    fn next_cursor_boundary(&self) -> Option<usize> {
        if self.cursor >= self.text.len() {
            return None;
        }
        self.text[self.cursor..]
            .chars()
            .next()
            .map(|ch| self.cursor + ch.len_utf8())
    }

    fn current_line_start(&self) -> usize {
        self.text[..self.cursor]
            .rfind('\n')
            .map(|idx| idx + 1)
            .unwrap_or(0)
    }

    fn current_line_end(&self) -> usize {
        self.text[self.cursor..]
            .find('\n')
            .map(|idx| self.cursor + idx)
            .unwrap_or(self.text.len())
    }

    fn display_width(&self, start: usize, end: usize) -> usize {
        self.text[start..end].chars().map(Self::char_width).sum()
    }

    fn char_width(ch: char) -> usize {
        if ch == '\t' {
            4
        } else {
            UnicodeWidthChar::width(ch).unwrap_or(0)
        }
    }
}
