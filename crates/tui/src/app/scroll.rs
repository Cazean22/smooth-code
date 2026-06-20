use super::*;

impl UiModel {
    pub(in crate::app) fn transcript_cache_width_hint(&self, terminal_width: u16) -> u16 {
        if self.screen == Screen::Workspace && terminal_width >= 110 && self.inspector_visible {
            let available = u32::from(terminal_width.saturating_sub(1));
            u16::try_from(available.saturating_mul(70) / 100).unwrap_or(u16::MAX)
        } else {
            terminal_width.max(1)
        }
    }

    pub(in crate::app) fn transcript_viewport_height(&self, width: u16, height: u16) -> u16 {
        if self.screen == Screen::Dashboard {
            return height.max(1);
        }
        // Full-screen subagent preview: everything except its header and the
        // key-hint footer.
        if !self.preview_stack.is_empty() {
            return height.saturating_sub(2).max(1);
        }

        let picker_height = self
            .question_picker
            .as_ref()
            .map(|picker| picker.desired_height(width).min(20))
            .unwrap_or(0);
        // The plan-approval overlay renders full-screen (its own branch in
        // `render`), so it no longer subtracts from the transcript viewport.
        let command_height = if self.mode == UiMode::Command { 1 } else { 0 };
        height
            .saturating_sub(picker_height)
            .saturating_sub(1)
            .saturating_sub(1)
            .saturating_sub(command_height)
            .saturating_sub(1)
            .saturating_sub(self.composer_height())
            .max(1)
    }

    pub(in crate::app) fn focus_next(&mut self) {
        self.focus = match self.focus {
            FocusTarget::Dashboard => FocusTarget::Transcript,
            FocusTarget::Transcript if self.inspector_visible => FocusTarget::Inspector,
            FocusTarget::Transcript => FocusTarget::Composer,
            FocusTarget::Inspector => FocusTarget::Composer,
            FocusTarget::Composer => FocusTarget::Transcript,
            FocusTarget::Overlay => FocusTarget::Transcript,
        };
    }

    pub(in crate::app) fn set_inspector_visible(&mut self, visible: bool) {
        self.inspector_visible = visible;
        if !visible && self.focus == FocusTarget::Inspector {
            self.focus = FocusTarget::Transcript;
        }
    }

    pub(in crate::app) fn toggle_inspector_visible(&mut self) {
        self.set_inspector_visible(!self.inspector_visible);
    }

    pub(in crate::app) fn dashboard_visible_item_count(&self, height: u16) -> usize {
        usize::from(height.saturating_sub(4) / 2).max(1)
    }

    pub(in crate::app) fn dashboard_max_scroll_offset(&self, visible_count: usize) -> usize {
        self.dashboard
            .items
            .len()
            .saturating_sub(visible_count.max(1))
    }

    pub(in crate::app) fn dashboard_ensure_selected_visible(&mut self, height: u16) {
        if self.dashboard.items.is_empty() {
            self.dashboard.selected = 0;
            self.dashboard.scroll_offset = 0;
            return;
        }

        self.dashboard.selected = self
            .dashboard
            .selected
            .min(self.dashboard.items.len().saturating_sub(1));

        let visible_count = self.dashboard_visible_item_count(height);
        if self.dashboard.selected < self.dashboard.scroll_offset {
            self.dashboard.scroll_offset = self.dashboard.selected;
        } else {
            let visible_end = self.dashboard.scroll_offset.saturating_add(visible_count);
            if self.dashboard.selected >= visible_end {
                self.dashboard.scroll_offset = self
                    .dashboard
                    .selected
                    .saturating_add(1)
                    .saturating_sub(visible_count);
            }
        }

        self.dashboard.scroll_offset = self
            .dashboard
            .scroll_offset
            .min(self.dashboard_max_scroll_offset(visible_count));
    }

    pub(in crate::app) fn scroll_up(&mut self, amount: u16) {
        self.scroll = self.scroll.saturating_sub(amount);
        self.auto_scroll = false;
    }

    /// Row range `(start, height)` of the item at `target_idx`, walking items
    /// exactly like `total_transcript_rows`: one separator row before each
    /// idx>0 item, cached heights, and the trailing user rule omitted when
    /// nothing streams below it.
    pub(in crate::app) fn transcript_item_row_extent(
        &mut self,
        target_idx: usize,
        width: u16,
    ) -> (usize, usize) {
        let item_count = self.transcript_items.len();
        let has_active_below = self.has_active_stream_lines();
        self.render_cache.evict_stale_widths(width);
        let mut rows = 0usize;
        for (idx, item) in self.transcript_items.iter().enumerate() {
            if idx > 0 {
                rows += 1;
            }
            let omit_bottom = idx + 1 == item_count && !has_active_below && item.is_user();
            let mut height = self.render_cache.item_height(item, width);
            if omit_bottom {
                height = height.saturating_sub(1);
            }
            if idx == target_idx {
                return (rows, height);
            }
            rows += height;
        }
        (rows, 0)
    }

    pub(in crate::app) fn transcript_selected_row_extent(
        &mut self,
        state: TranscriptSelectState,
        width: u16,
    ) -> (usize, usize) {
        let (item_start, item_height) = self.transcript_item_row_extent(state.selected, width);
        let Some(entry_idx) = state.selected_tool_entry else {
            return (item_start, item_height);
        };
        let Some(group) = self
            .transcript_items
            .get(state.selected)
            .and_then(|item| item.tool_group_cell())
        else {
            return (item_start, item_height);
        };
        let Some((entry_start, entry_height)) =
            group.entry_row_extent(usize::from(width.max(1)), entry_idx)
        else {
            return (item_start, item_height);
        };
        (item_start.saturating_add(entry_start), entry_height)
    }

    /// Scroll just enough that the selected transcript row is on screen;
    /// selected entries inside batched tool rows scroll independently. Items or
    /// entries taller than the viewport pin to their top.
    pub(in crate::app) fn transcript_select_ensure_visible(&mut self, viewport_height: u16) {
        let Some(state) = self.transcript_select else {
            return;
        };
        let (start, height) =
            self.transcript_selected_row_extent(state, self.transcript_inner_width);
        let vp = usize::from(viewport_height.max(1));
        let scroll = usize::from(self.scroll);
        let mut new_scroll = if start < scroll {
            start
        } else if start.saturating_add(height) > scroll.saturating_add(vp) {
            start.saturating_add(height).saturating_sub(vp).min(start)
        } else {
            scroll
        };
        new_scroll = new_scroll.min(usize::from(self.max_scroll(viewport_height)));
        self.scroll = u16::try_from(new_scroll).unwrap_or(u16::MAX);
    }

    pub(in crate::app) fn scroll_down(&mut self, amount: u16, viewport_height: u16) {
        let max_scroll = self.max_scroll(viewport_height);
        self.scroll = self.scroll.saturating_add(amount).min(max_scroll);
        self.auto_scroll = self.scroll >= max_scroll;
    }

    pub(in crate::app) fn scroll_to_bottom(&mut self, viewport_height: u16) {
        self.scroll = self.max_scroll(viewport_height);
    }

    pub(in crate::app) fn max_scroll(&mut self, viewport_height: u16) -> u16 {
        let inner_width = self.transcript_inner_width;
        let total_rows = self.total_transcript_rows(inner_width);
        let max_scroll = total_rows.saturating_sub(usize::from(viewport_height));
        u16::try_from(max_scroll).unwrap_or(u16::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::*;

    #[test]
    fn inspector_commands_hide_show_and_toggle_visibility() {
        let mut model = UiModel::new();

        assert!(model.inspector_visible);

        let effects = model.execute_command("inspector hide");
        assert!(effects.is_empty());
        assert!(!model.inspector_visible);

        let effects = model.execute_command("inspector show");
        assert!(effects.is_empty());
        assert!(model.inspector_visible);

        let effects = model.execute_command("inspector toggle");
        assert!(effects.is_empty());
        assert!(!model.inspector_visible);

        let effects = model.execute_command("inspector");
        assert!(effects.is_empty());
        assert!(model.inspector_visible);
    }

    #[test]
    fn normal_mode_uppercase_i_toggles_inspector_visibility() {
        let mut model = workspace_normal_model();

        let effects = model.handle_key_event(key(KeyCode::Char('I')));
        assert!(effects.is_empty());
        assert!(!model.inspector_visible);

        let effects = model.handle_key_event(key(KeyCode::Char('I')));
        assert!(effects.is_empty());
        assert!(model.inspector_visible);
    }

    #[test]
    fn hidden_inspector_is_skipped_by_tab() {
        let mut model = workspace_normal_model();
        model.inspector_visible = false;

        let _ = model.handle_key_event(key(KeyCode::Tab));
        assert_eq!(model.focus, FocusTarget::Composer);

        let _ = model.handle_key_event(key(KeyCode::Tab));
        assert_eq!(model.focus, FocusTarget::Transcript);
    }

    #[test]
    fn hiding_focused_inspector_falls_back_to_transcript() {
        let mut model = workspace_normal_model();
        model.focus = FocusTarget::Inspector;

        let effects = model.execute_command("inspector hide");

        assert!(effects.is_empty());
        assert!(!model.inspector_visible);
        assert_eq!(model.focus, FocusTarget::Transcript);
    }

    #[test]
    fn wide_workspace_defaults_to_transcript_and_inspector()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut model = workspace_normal_model();

        let mut terminal = Terminal::new(TestBackend::new(120, 24))?;
        terminal.draw(|frame| model.render(frame))?;

        let rendered = rendered_buffer_text(&terminal);
        assert!(
            rendered.contains("No transcript yet. Type a message and use :send."),
            "{rendered}"
        );
        assert!(rendered.contains("│"), "{rendered}");
        assert!(rendered.contains("Inspector"), "{rendered}");
        Ok(())
    }

    #[test]
    fn focus_inspector_command_restores_inspector_visibility_on_wide_workspace()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut model = workspace_normal_model();
        let _ = model.execute_command("inspector hide");

        let effects = model.execute_command("focus inspector");

        assert!(effects.is_empty());
        assert!(model.inspector_visible);
        assert_eq!(model.focus, FocusTarget::Inspector);

        let mut terminal = Terminal::new(TestBackend::new(120, 24))?;
        terminal.draw(|frame| model.render(frame))?;

        let rendered = rendered_buffer_text(&terminal);
        assert!(rendered.contains("Inspector"), "{rendered}");
        Ok(())
    }

    #[test]
    fn dashboard_down_keeps_selected_item_visible_by_scrolling() {
        let mut model = UiModel::new();
        model.viewport_height = 10;
        model.dashboard.items = (0..8).map(dashboard_thread).collect();

        for _ in 0..3 {
            let _ = model.handle_key_event(key(KeyCode::Down));
        }

        assert_eq!(model.dashboard.selected, 3);
        assert_eq!(model.dashboard.scroll_offset, 1);

        let _ = model.handle_key_event(key(KeyCode::Up));
        assert_eq!(model.dashboard.selected, 2);
        assert_eq!(model.dashboard.scroll_offset, 1);

        let _ = model.handle_key_event(key(KeyCode::Up));
        assert_eq!(model.dashboard.selected, 1);
        assert_eq!(model.dashboard.scroll_offset, 1);

        let _ = model.handle_key_event(key(KeyCode::Up));
        assert_eq!(model.dashboard.selected, 0);
        assert_eq!(model.dashboard.scroll_offset, 0);
    }

    #[test]
    fn dashboard_page_home_end_update_scroll_offset() {
        let mut model = UiModel::new();
        model.viewport_height = 8;
        model.dashboard.items = (0..10).map(dashboard_thread).collect();

        let _ = model.handle_key_event(key(KeyCode::PageDown));
        assert_eq!(model.dashboard.selected, 2);
        assert_eq!(model.dashboard.scroll_offset, 1);

        let _ = model.handle_key_event(key(KeyCode::End));
        assert_eq!(model.dashboard.selected, 9);
        assert_eq!(model.dashboard.scroll_offset, 8);

        let _ = model.handle_key_event(key(KeyCode::PageUp));
        assert_eq!(model.dashboard.selected, 7);
        assert_eq!(model.dashboard.scroll_offset, 7);

        let _ = model.handle_key_event(key(KeyCode::Home));
        assert_eq!(model.dashboard.selected, 0);
        assert_eq!(model.dashboard.scroll_offset, 0);
    }
}
