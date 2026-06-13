//! Full-screen, read-only live view of a subagent session, opened with `gd`
//! from transcript-select mode. Views stack to support nested subagents.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use app_server_protocol::ThreadPreviewResponse;
use ratatui::{
    style::{Color, Style},
    text::Line,
};
use smooth_protocol::{AgentStatus, EventMsg, ThreadId, ToolCallResultKind};

use crate::app::{ActiveWrap, RenderedTranscriptCache, agent_status_label, append_visible_line};
use crate::history_cell::{ToolCallGroupCell, ToolCallState, TranscriptItem, TranscriptItemId};
use crate::streaming::StreamController;

/// One stacked preview of a subagent thread. A reduced, self-contained
/// counterpart of `UiModel`'s transcript state: it replays the thread's
/// persisted events and then applies its live broadcast events, but never
/// reacts to UI-global concerns (pickers, modes, effects).
pub(crate) struct SubagentPreviewView {
    pub(crate) thread_id: ThreadId,
    agent_path: Option<String>,
    agent_nickname: Option<String>,
    pub(crate) status: AgentStatus,
    pub(crate) is_live: bool,
    items: Vec<TranscriptItem>,
    next_item_id: TranscriptItemId,
    /// call_id -> (item index, entry index); never cleared, so a duplicate
    /// `ToolCallStarted` from the subscribe-then-snapshot overlap is dropped.
    tool_call_rows: HashMap<String, (usize, usize)>,
    /// Final completions already applied; a duplicate `ToolCallCompleted`
    /// must not fall into the "unknown call" info/error fallback.
    completed_tool_call_ids: HashSet<String>,
    pending_tool_group: Option<(usize, String)>,
    committed_assistant_item_id: Option<String>,
    committed_reasoning_item_id: Option<String>,
    assistant_stream: Option<StreamController>,
    reasoning_stream: Option<StreamController>,
    active_assistant_lines: Option<Vec<Line<'static>>>,
    active_reasoning_lines: Option<Vec<Line<'static>>>,
    active_version: u64,
    active_wrap: Option<ActiveWrap>,
    pub(crate) selected: usize,
    /// Armed by `g` for the `gg`/`gd` chord inside the preview.
    pub(crate) pending_g: Option<Instant>,
    pub(crate) scroll: u16,
    pub(crate) auto_scroll: bool,
    render_cache: RenderedTranscriptCache,
}

impl SubagentPreviewView {
    pub(crate) fn from_preview_response(response: ThreadPreviewResponse, width: u16) -> Self {
        let thread_id = response
            .thread_id
            .parse::<ThreadId>()
            .unwrap_or_else(|_| ThreadId::new());
        let mut view = Self {
            thread_id,
            agent_path: response.agent_path,
            agent_nickname: response.agent_nickname,
            status: response.status.clone(),
            is_live: response.is_live,
            items: Vec::new(),
            next_item_id: 0,
            tool_call_rows: HashMap::new(),
            completed_tool_call_ids: HashSet::new(),
            pending_tool_group: None,
            committed_assistant_item_id: None,
            committed_reasoning_item_id: None,
            assistant_stream: None,
            reasoning_stream: None,
            active_assistant_lines: None,
            active_reasoning_lines: None,
            active_version: 0,
            active_wrap: None,
            selected: 0,
            pending_g: None,
            scroll: 0,
            auto_scroll: true,
            render_cache: RenderedTranscriptCache::default(),
        };
        for msg in response.initial_messages {
            view.apply_event(msg, width);
        }
        // Replayed events may lag the live status (and a live thread has no
        // terminal event at all), so the response status wins.
        view.status = response.status;
        view.selected = view.items.len().saturating_sub(1);
        view
    }

    pub(crate) fn header_label(&self) -> String {
        let identity = self
            .agent_nickname
            .as_deref()
            .or(self.agent_path.as_deref())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| self.thread_id.to_string());
        format!("subagent {identity} — {}", agent_status_label(&self.status))
    }

    /// The subagent spawned by the currently selected row, for nested `gd`.
    pub(crate) fn selected_tool_group(&self) -> Option<&ToolCallGroupCell> {
        self.items
            .get(self.selected)
            .and_then(|i| i.tool_group_cell())
    }

    pub(crate) fn item_count(&self) -> usize {
        self.items.len()
    }

    #[cfg(test)]
    pub(crate) fn items(&self) -> &[TranscriptItem] {
        &self.items
    }

    /// Apply one protocol event from this thread (replayed or live). A
    /// reduced port of `UiModel::apply_protocol_event`, with two deliberate
    /// differences: completed full-text events win over the active streamed
    /// tail (a preview opened mid-stream has no earlier deltas, so committing
    /// the tail would truncate the message), and duplicate started/completed
    /// tool calls are dropped (subscribe-then-snapshot can duplicate
    /// persisted events).
    pub(crate) fn apply_event(&mut self, msg: EventMsg, width: u16) {
        self.update_status_from_event(&msg);
        match msg {
            EventMsg::UserMessage { text } => {
                self.finalize_reasoning_stream();
                let id = self.next_item_id();
                self.push_history(TranscriptItem::user(id, text));
            }
            EventMsg::AgentMessage { text } => {
                self.finalize_reasoning_stream();
                self.push_rendered_assistant_message(&text, width);
            }
            EventMsg::AgentMessageDelta(delta) => {
                if self.assistant_stream.is_none() {
                    self.assistant_stream = Some(StreamController::new(Some(usize::from(
                        width.saturating_sub(6).max(20),
                    ))));
                }
                let snapshot = self.assistant_stream.as_mut().and_then(|controller| {
                    let _ = controller.push(&delta.delta);
                    controller.snapshot_lines()
                });
                self.set_active_assistant_lines(snapshot);
            }
            EventMsg::AgentMessageCompleted(completed) => {
                if self.committed_assistant_item_id.as_deref() == Some(completed.item_id.as_str()) {
                    return;
                }
                // Full text wins: discard the (possibly tail-only) stream.
                self.assistant_stream = None;
                self.set_active_assistant_lines(None);
                self.push_rendered_assistant_message(&completed.text, width);
                self.committed_assistant_item_id = Some(completed.item_id);
            }
            EventMsg::AgentReasoningDelta(delta) => {
                self.pending_tool_group = None;
                if self.reasoning_stream.is_none() {
                    self.reasoning_stream = Some(StreamController::new(Some(usize::from(
                        width.saturating_sub(6).max(20),
                    ))));
                }
                let snapshot = self.reasoning_stream.as_mut().and_then(|controller| {
                    let _ = controller.push(&delta.delta);
                    controller.snapshot_lines()
                });
                self.set_active_reasoning_lines(snapshot);
            }
            EventMsg::AgentReasoningCompleted(completed) => {
                self.pending_tool_group = None;
                if self.committed_reasoning_item_id.as_deref() == Some(completed.item_id.as_str()) {
                    return;
                }
                self.reasoning_stream = None;
                self.set_active_reasoning_lines(None);
                self.push_rendered_reasoning_message(&completed.text, width);
                self.committed_reasoning_item_id = Some(completed.item_id);
            }
            EventMsg::ToolCallStarted(tool) => {
                if self.tool_call_rows.contains_key(&tool.call_id) {
                    return;
                }
                self.finalize_assistant_stream();
                if let Some(cell_idx) = self
                    .pending_tool_group
                    .as_ref()
                    .and_then(|(idx, name)| (name == &tool.tool_name).then_some(*idx))
                {
                    let entry_idx = self
                        .tool_group_mut(cell_idx)
                        .map(|group| group.push_entry(tool.args_preview))
                        .unwrap_or(0);
                    self.mark_item_mutated(cell_idx);
                    self.tool_call_rows
                        .insert(tool.call_id, (cell_idx, entry_idx));
                } else {
                    let cell_idx = self.items.len();
                    let id = self.next_item_id();
                    let cell = ToolCallGroupCell::new(tool.tool_name.clone(), tool.args_preview);
                    self.items.push(TranscriptItem::tool_group(id, cell));
                    self.pending_tool_group = Some((cell_idx, tool.tool_name));
                    self.tool_call_rows.insert(tool.call_id, (cell_idx, 0));
                }
            }
            EventMsg::ToolCallCompleted(tool) => {
                self.finalize_assistant_stream();
                let row = self.tool_call_rows.get(&tool.call_id).copied();
                if let Some(thread_id) = tool.related_thread_id
                    && let Some((cell_idx, entry_idx)) = row
                {
                    if let Some(group) = self.tool_group_mut(cell_idx) {
                        group.set_entry_related_thread(entry_idx, thread_id);
                    }
                    self.mark_item_mutated(cell_idx);
                }
                if tool.result_kind == ToolCallResultKind::StatusUpdate && tool.success {
                    if let Some((cell_idx, entry_idx)) = row {
                        if let Some(group) = self.tool_group_mut(cell_idx) {
                            group.set_entry_outcome(entry_idx, ToolCallState::Running, None);
                        }
                        self.mark_item_mutated(cell_idx);
                    }
                    return;
                }
                if !self.completed_tool_call_ids.insert(tool.call_id.clone()) {
                    return;
                }
                // Patch/todo events are rendered as plain tool rows here; the
                // structured replacements stay a main-transcript affordance.
                let state = if tool.success {
                    ToolCallState::Success
                } else {
                    ToolCallState::Failure
                };
                let error = if tool.success {
                    None
                } else {
                    Some(tool.error.unwrap_or_else(|| String::from("tool failed")))
                };
                if let Some((cell_idx, entry_idx)) = row {
                    if let Some(group) = self.tool_group_mut(cell_idx) {
                        if let Some(output) = tool.output_preview {
                            group.set_entry_output(entry_idx, output);
                        }
                        group.set_entry_outcome(entry_idx, state, error);
                    }
                    self.mark_item_mutated(cell_idx);
                } else if let Some(error) = error {
                    let id = self.next_item_id();
                    self.push_history(TranscriptItem::error(
                        id,
                        format!("tool {} failed: {error}", tool.call_id),
                    ));
                }
            }
            EventMsg::TurnStarted(_) => {
                self.pending_tool_group = None;
            }
            EventMsg::TurnCompleted(_) | EventMsg::TurnInterrupted(_) => {
                self.finalize_assistant_stream();
            }
            EventMsg::Error(error) => {
                self.finalize_assistant_stream();
                let id = self.next_item_id();
                self.push_history(TranscriptItem::error(id, error.error.message));
            }
            EventMsg::CollabAgentCompleted(event) => {
                let id = self.next_item_id();
                let message = format_args_completed(&event);
                self.push_history(TranscriptItem::info(id, message));
            }
            EventMsg::CollabAgentSpawnEnd(event) => {
                if let Some(thread_id) = event.new_thread_id
                    && let Some((cell_idx, entry_idx)) =
                        self.tool_call_rows.get(&event.call_id).copied()
                {
                    if let Some(group) = self.tool_group_mut(cell_idx) {
                        group.set_entry_related_thread(entry_idx, thread_id);
                    }
                    self.mark_item_mutated(cell_idx);
                }
            }
            // UI-global or cosmetic events have no preview rendering.
            _ => {}
        }
    }

    /// Mirror of core's `agent_status_from_event`, applied to every event so
    /// the header tracks terminal turn states even when no explicit
    /// `AgentStatusChanged` arrives on this channel.
    fn update_status_from_event(&mut self, msg: &EventMsg) {
        let status = match msg {
            EventMsg::AgentStatusChanged(event) => Some(event.status.clone()),
            EventMsg::TurnStarted(_) => Some(AgentStatus::Running),
            EventMsg::TurnCompleted(event) => {
                Some(AgentStatus::Completed(event.last_assistant_message.clone()))
            }
            EventMsg::TurnInterrupted(_) => Some(AgentStatus::Interrupted),
            EventMsg::Error(event) => Some(AgentStatus::Errored(event.error.clone())),
            _ => None,
        };
        if let Some(status) = status {
            if matches!(
                status,
                AgentStatus::Interrupted
                    | AgentStatus::Completed(_)
                    | AgentStatus::Errored(_)
                    | AgentStatus::Shutdown
            ) {
                self.is_live = false;
            }
            self.status = status;
        }
    }

    /// Patch the status from the parent's `CollabAgentCompleted` (the child's
    /// own channel may close before its final status broadcast arrives).
    pub(crate) fn complete_from_parent(&mut self, status: AgentStatus) {
        self.status = status;
        self.is_live = false;
    }

    fn next_item_id(&mut self) -> TranscriptItemId {
        let id = self.next_item_id;
        self.next_item_id = self.next_item_id.saturating_add(1);
        id
    }

    fn push_history(&mut self, item: TranscriptItem) {
        self.pending_tool_group = None;
        self.items.push(item);
    }

    fn tool_group_mut(&mut self, cell_idx: usize) -> Option<&mut ToolCallGroupCell> {
        self.items
            .get_mut(cell_idx)
            .and_then(TranscriptItem::tool_group_mut)
    }

    fn mark_item_mutated(&mut self, cell_idx: usize) {
        if let Some(item) = self.items.get_mut(cell_idx) {
            item.mark_mutated();
        }
    }

    fn push_rendered_assistant_message(&mut self, message: &str, width: u16) {
        let lines = render_markdown(message, width);
        let id = self.next_item_id();
        self.push_history(TranscriptItem::assistant(
            id,
            lines,
            true,
            message.to_owned(),
        ));
    }

    fn push_rendered_reasoning_message(&mut self, message: &str, width: u16) {
        let lines = render_markdown(message, width);
        let id = self.next_item_id();
        self.push_history(TranscriptItem::reasoning(
            id,
            lines,
            true,
            message.to_owned(),
        ));
    }

    fn set_active_assistant_lines(&mut self, lines: Option<Vec<Line<'static>>>) {
        self.active_assistant_lines = lines;
        self.active_version = self.active_version.wrapping_add(1);
    }

    fn set_active_reasoning_lines(&mut self, lines: Option<Vec<Line<'static>>>) {
        self.active_reasoning_lines = lines;
        self.active_version = self.active_version.wrapping_add(1);
    }

    /// Commit any active streams as items. Used for turn boundaries and tool
    /// starts; the *Completed handlers instead discard the stream because the
    /// full text follows.
    fn finalize_assistant_stream(&mut self) {
        self.finalize_reasoning_stream();
        if let Some(controller) = self.assistant_stream.take() {
            self.pending_tool_group = None;
            if let Some((lines, raw)) = controller.finalize_parts() {
                let id = self.next_item_id();
                self.push_history(TranscriptItem::assistant(id, lines, true, raw));
            }
        }
        self.set_active_assistant_lines(None);
    }

    fn finalize_reasoning_stream(&mut self) {
        if let Some(controller) = self.reasoning_stream.take()
            && let Some((lines, raw)) = controller.finalize_parts()
        {
            let id = self.next_item_id();
            self.push_history(TranscriptItem::reasoning(id, lines, true, raw));
        }
        self.set_active_reasoning_lines(None);
    }

    fn refresh_active_wrap(&mut self, width: u16) {
        if self
            .active_wrap
            .as_ref()
            .is_some_and(|cache| cache.width == width && cache.version == self.active_version)
        {
            return;
        }
        let reasoning = self
            .active_reasoning_lines
            .as_ref()
            .map(|lines| {
                TranscriptItem::reasoning(0, lines.clone(), true, String::new())
                    .display_lines(width)
            })
            .unwrap_or_default();
        let assistant = self
            .active_assistant_lines
            .as_ref()
            .map(|lines| {
                TranscriptItem::assistant(0, lines.clone(), true, String::new())
                    .display_lines(width)
            })
            .unwrap_or_default();
        self.active_wrap = Some(ActiveWrap {
            width,
            version: self.active_version,
            reasoning,
            assistant,
        });
    }

    pub(crate) fn total_rows(&mut self, width: u16) -> usize {
        if self.items.is_empty()
            && self.active_assistant_lines.is_none()
            && self.active_reasoning_lines.is_none()
        {
            return 1;
        }
        self.render_cache.evict_stale_widths(width);
        let mut rows = 0usize;
        for (idx, item) in self.items.iter().enumerate() {
            if idx > 0 {
                rows += 1;
            }
            rows += self.render_cache.item_height(item, width);
        }
        self.refresh_active_wrap(width);
        let Some(active) = self.active_wrap.as_ref() else {
            return rows;
        };
        if !active.reasoning.is_empty() {
            if rows > 0 {
                rows += 1;
            }
            rows += active.reasoning.len();
        }
        if !active.assistant.is_empty() {
            if rows > 0 {
                rows += 1;
            }
            rows += active.assistant.len();
        }
        rows
    }

    pub(crate) fn max_scroll(&mut self, width: u16, viewport_height: u16) -> u16 {
        let total_rows = self.total_rows(width);
        u16::try_from(total_rows.saturating_sub(usize::from(viewport_height))).unwrap_or(u16::MAX)
    }

    pub(crate) fn scroll_to_bottom(&mut self, width: u16, viewport_height: u16) {
        self.scroll = self.max_scroll(width, viewport_height);
    }

    /// Row range `(start, height)` of the item at `target_idx`.
    fn item_row_extent(&mut self, target_idx: usize, width: u16) -> (usize, usize) {
        self.render_cache.evict_stale_widths(width);
        let mut rows = 0usize;
        for (idx, item) in self.items.iter().enumerate() {
            if idx > 0 {
                rows += 1;
            }
            let height = self.render_cache.item_height(item, width);
            if idx == target_idx {
                return (rows, height);
            }
            rows += height;
        }
        (rows, 0)
    }

    pub(crate) fn ensure_selected_visible(&mut self, width: u16, viewport_height: u16) {
        let (start, height) = self.item_row_extent(self.selected, width);
        let vp = usize::from(viewport_height.max(1));
        let scroll = usize::from(self.scroll);
        let mut new_scroll = if start < scroll {
            start
        } else if start.saturating_add(height) > scroll.saturating_add(vp) {
            start.saturating_add(height).saturating_sub(vp).min(start)
        } else {
            scroll
        };
        new_scroll = new_scroll.min(usize::from(self.max_scroll(width, viewport_height)));
        self.scroll = u16::try_from(new_scroll).unwrap_or(u16::MAX);
        self.auto_scroll = false;
    }

    pub(crate) fn visible_lines(&mut self, width: u16, viewport_height: u16) -> Vec<Line<'static>> {
        let start = usize::from(self.scroll);
        let end = usize::from(self.scroll).saturating_add(usize::from(viewport_height));
        let mut all_rows = 0usize;
        let mut lines = Vec::new();
        let selected_idx = self.selected;

        self.render_cache.evict_stale_widths(width);
        for (idx, item) in self.items.iter().enumerate() {
            if idx > 0 {
                append_visible_line(&mut lines, Line::default(), all_rows, start, end);
                all_rows += 1;
            }
            let item_height = self.render_cache.item_height(item, width);
            if all_rows >= end || all_rows.saturating_add(item_height) <= start {
                all_rows = all_rows.saturating_add(item_height);
                continue;
            }
            let mut item_lines = self.render_cache.item_lines(item, width);
            if selected_idx == idx {
                for line in &mut item_lines {
                    line.style = line.style.patch(Style::default().bg(Color::DarkGray));
                }
            }
            for line in item_lines {
                append_visible_line(&mut lines, line, all_rows, start, end);
                all_rows += 1;
            }
        }

        self.refresh_active_wrap(width);
        if let Some(active) = self.active_wrap.as_ref() {
            let has_reasoning = !active.reasoning.is_empty();
            let has_assistant = !active.assistant.is_empty();
            if (has_reasoning || has_assistant) && all_rows > 0 {
                append_visible_line(&mut lines, Line::default(), all_rows, start, end);
                all_rows += 1;
            }
            for line in &active.reasoning {
                append_visible_line(&mut lines, line.clone(), all_rows, start, end);
                all_rows += 1;
            }
            if has_reasoning && has_assistant {
                append_visible_line(&mut lines, Line::default(), all_rows, start, end);
                all_rows += 1;
            }
            for line in &active.assistant {
                append_visible_line(&mut lines, line.clone(), all_rows, start, end);
                all_rows += 1;
            }
        }

        if all_rows == 0 {
            lines.push(Line::from("Subagent transcript is empty.").style(Style::default().dim()));
        }
        lines
    }
}

fn render_markdown(message: &str, width: u16) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    crate::markdown::append_markdown(
        message,
        Some(usize::from(width.saturating_sub(6))),
        &mut lines,
    );
    lines
}

fn format_args_completed(event: &smooth_protocol::CollabAgentCompletedEvent) -> String {
    let nickname = event
        .agent_nickname
        .as_deref()
        .unwrap_or_else(|| event.agent_path.as_str());
    format!(
        "nested subagent {nickname} finished: {}",
        agent_status_label(&event.status)
    )
}
