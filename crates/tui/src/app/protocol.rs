use super::*;

impl UiModel {
    pub(in crate::app) fn apply_protocol_event(&mut self, event: Event) {
        match event.msg {
            EventMsg::SessionConfigured(configured) => {
                self.finalize_reasoning_stream();
                match configured.thread_id.parse::<ThreadId>() {
                    Ok(thread_id) => {
                        self.current_thread_id = Some(thread_id);
                        self.screen = Screen::Workspace;
                        self.push_info(format!(
                            "Session configured for thread {}",
                            configured.thread_id
                        ));
                    }
                    Err(err) => {
                        self.push_error(format!("Invalid configured thread id: {err}"));
                    }
                }
            }
            EventMsg::TurnStarted(turn) => {
                self.is_turn_running = true;
                self.is_turn_cancelling = false;
                self.tool_call_rows.clear();
                self.subagent_tool_calls.clear();
                self.running_tools.clear();
                self.pending_tool_group = None;
                self.current_turn_id = Some(turn.turn_id.clone());
                self.committed_assistant_item_id = None;
                self.committed_assistant_for_current_turn = false;
                self.committed_reasoning_item_id = None;
                self.in_flight_reasoning_item_id = None;
                self.status_line = format!("Running turn {}", turn.turn_id);
            }
            EventMsg::TurnCompleted(turn) => {
                let committed_from_stream = self.finalize_assistant_stream(None);
                self.is_turn_running = false;
                self.is_turn_cancelling = false;
                self.running_tools.clear();
                self.status_line = format!("Completed turn {}", turn.turn_id);
                if let Some(message) = turn.last_assistant_message
                    && !committed_from_stream
                    && !self.committed_assistant_for_current_turn
                {
                    self.push_rendered_assistant_message(&message);
                    self.committed_assistant_for_current_turn = true;
                }
            }
            EventMsg::TurnInterrupted(turn) => {
                self.finalize_assistant_stream(None);
                self.is_turn_running = false;
                self.is_turn_cancelling = false;
                self.running_tools.clear();
                self.question_picker = None;
                self.plan_approval = None;
                self.exit_transcript_select();
                if self.mode == UiMode::Overlay {
                    self.mode = UiMode::Normal;
                    self.focus = FocusTarget::Transcript;
                }
                self.push_info(format!(
                    "Turn {} interrupted: {}",
                    turn.turn_id, turn.reason
                ));
                self.status_line = String::from("Interrupted");
            }
            EventMsg::AgentStatusChanged(status) => {
                self.status_line = format!("Status: {}", agent_status_label(&status.status));
            }
            EventMsg::UserMessage { text } => {
                self.finalize_reasoning_stream();
                let id = self.next_item_id();
                self.push_history(TranscriptItem::user(id, text));
            }
            EventMsg::AgentMessage { text } => {
                self.finalize_reasoning_stream();
                if !self.committed_assistant_for_current_turn {
                    self.push_rendered_assistant_message(&text);
                    self.committed_assistant_for_current_turn = true;
                }
            }
            EventMsg::AgentMessageDelta(delta) => {
                self.handle_assistant_delta(delta.delta);
            }
            EventMsg::AgentMessageCompleted(completed) => {
                let committed_from_stream =
                    self.finalize_assistant_stream(Some(completed.item_id.as_str()));
                if !committed_from_stream
                    && self.committed_assistant_item_id.as_deref()
                        != Some(completed.item_id.as_str())
                {
                    self.push_rendered_assistant_message(&completed.text);
                    self.committed_assistant_item_id = Some(completed.item_id);
                    self.committed_assistant_for_current_turn = true;
                }
            }
            EventMsg::AgentReasoningDelta(delta) => {
                self.handle_reasoning_delta(delta.item_id, delta.delta);
            }
            EventMsg::AgentReasoningCompleted(completed) => {
                self.pending_tool_group = None;
                if self.committed_reasoning_item_id.as_deref() != Some(completed.item_id.as_str()) {
                    let committed_from_stream = self.finalize_reasoning_stream();
                    if !committed_from_stream {
                        self.push_rendered_reasoning_message(&completed.text);
                        self.committed_reasoning_item_id = Some(completed.item_id);
                    }
                }
            }
            EventMsg::ToolCallStarted(tool) => {
                self.finalize_assistant_stream(None);

                let call_id = tool.call_id;
                let tool_name = tool.tool_name;
                let args_preview = tool.args_preview;
                self.running_tools.insert(
                    call_id.clone(),
                    RunningToolInfo {
                        tool_name: tool_name.clone(),
                        args_preview: args_preview.clone(),
                    },
                );

                if let Some(cell_idx) = self
                    .pending_tool_group
                    .as_ref()
                    .and_then(|(idx, name)| (name == &tool_name).then_some(*idx))
                {
                    let entry_idx = self.tool_group_mut(cell_idx).push_entry(args_preview);
                    self.mark_item_mutated(cell_idx);
                    self.tool_call_rows.insert(call_id, (cell_idx, entry_idx));
                } else {
                    let cell_idx =
                        self.push_tool_group_cell(ToolCallGroupCell::new(tool_name, args_preview));
                    self.tool_call_rows.insert(call_id, (cell_idx, 0));
                }
            }
            EventMsg::ToolCallCompleted(tool) => {
                self.finalize_assistant_stream(None);
                // Record the spawned child on the tool row (both the live
                // StatusUpdate and the fast-finish Final paths carry it), so
                // Enter can resolve the row to a subagent session later.
                if let Some(thread_id) = tool.related_thread_id
                    && let Some((cell_idx, entry_idx)) =
                        self.tool_call_rows.get(&tool.call_id).copied()
                {
                    self.tool_group_mut(cell_idx)
                        .set_entry_related_thread(entry_idx, thread_id);
                    self.mark_item_mutated(cell_idx);
                }
                if tool.result_kind == ToolCallResultKind::StatusUpdate && tool.success {
                    if let Some(thread_id) = tool.related_thread_id {
                        self.subagent_tool_calls
                            .insert(thread_id, tool.call_id.clone());
                    }
                    if let Some((cell_idx, entry_idx)) =
                        self.tool_call_rows.get(&tool.call_id).copied()
                    {
                        self.tool_group_mut(cell_idx).set_entry_outcome(
                            entry_idx,
                            ToolCallState::Running,
                            None,
                        );
                        self.mark_item_mutated(cell_idx);
                    }
                    return;
                }

                self.running_tools.remove(&tool.call_id);
                let output_preview = tool.output_preview;
                let handled_structured = if tool.success {
                    if tool.todos.is_empty() {
                        let file_changes = if tool.file_changes.is_empty() {
                            tool.file_change.into_iter().collect()
                        } else {
                            tool.file_changes
                        };
                        self.replace_tool_call_with_file_changes(&tool.call_id, file_changes)
                    } else {
                        self.replace_tool_call_with_todo_list(&tool.call_id, tool.todos)
                    }
                } else {
                    false
                };

                if !handled_structured {
                    let new_state = if tool.success {
                        ToolCallState::Success
                    } else {
                        ToolCallState::Failure
                    };
                    let error = if tool.success {
                        None
                    } else {
                        Some(tool.error.unwrap_or_else(|| String::from("tool failed")))
                    };

                    if let Some((cell_idx, entry_idx)) = self.tool_call_rows.remove(&tool.call_id) {
                        if let Some(output) = output_preview {
                            self.tool_group_mut(cell_idx)
                                .set_entry_output(entry_idx, output);
                        }
                        self.tool_group_mut(cell_idx)
                            .set_entry_outcome(entry_idx, new_state, error);
                        self.mark_item_mutated(cell_idx);
                    } else if let Some(error) = error {
                        self.push_error(format!("tool {} failed: {}", tool.call_id, error));
                    } else {
                        self.push_info(format!("tool {} completed", tool.call_id));
                    }
                }
            }
            EventMsg::Error(error) => {
                self.finalize_assistant_stream(None);
                self.push_error(error.error.message);
                self.is_turn_running = false;
                self.is_turn_cancelling = false;
                self.running_tools.clear();
                self.status_line = String::from("Error");
            }
            EventMsg::StreamError(error) => {
                self.finalize_assistant_stream(None);
                self.push_info(error.message);
            }
            EventMsg::CollabAgentSpawnBegin(_event) => {}
            EventMsg::CollabAgentSpawnEnd(event) => {
                if let Some(thread_id) = event.new_thread_id
                    && let Some((cell_idx, entry_idx)) =
                        self.tool_call_rows.get(&event.call_id).copied()
                {
                    self.tool_group_mut(cell_idx)
                        .set_entry_related_thread(entry_idx, thread_id);
                    self.mark_item_mutated(cell_idx);
                }
            }
            EventMsg::CollabAgentCompleted(event) => {
                self.complete_subagent_tool_call(event.child_thread_id, &event.status);
            }
            EventMsg::CollabResumeBegin(_event) => {}
            EventMsg::CollabResumeEnd(_event) => {}
            EventMsg::PlanModeChanged(event) => {
                // Resume replays historical PlanModeChanged events to restore
                // the badge; only narrate actual state changes.
                if self.plan_mode != event.enabled {
                    let message = if event.enabled {
                        "plan mode enabled — file mutations are locked while the agent plans"
                    } else {
                        "plan mode disabled — agent back to the full tool set"
                    };
                    self.push_info(message);
                }
                self.plan_mode = event.enabled;
            }
        }
    }

    pub(in crate::app) fn handle_assistant_delta(&mut self, delta: String) {
        if self.assistant_stream.is_none() {
            self.assistant_stream = Some(StreamController::new(Some(usize::from(
                self.terminal_width.saturating_sub(6).max(20),
            ))));
        }
        let snapshot = self.assistant_stream.as_mut().and_then(|controller| {
            let _ = controller.push(&delta);
            controller.snapshot_lines()
        });
        self.set_active_assistant_lines(snapshot);
    }

    pub(in crate::app) fn handle_reasoning_delta(&mut self, item_id: String, delta: String) {
        self.pending_tool_group = None;
        self.in_flight_reasoning_item_id = Some(item_id);
        if self.reasoning_stream.is_none() {
            self.reasoning_stream = Some(StreamController::new(Some(usize::from(
                self.terminal_width.saturating_sub(6).max(20),
            ))));
        }
        let snapshot = self.reasoning_stream.as_mut().and_then(|controller| {
            let _ = controller.push(&delta);
            controller.snapshot_lines()
        });
        self.set_active_reasoning_lines(snapshot);
    }

    pub(in crate::app) fn finalize_assistant_stream(&mut self, item_id: Option<&str>) -> bool {
        self.finalize_reasoning_stream();
        self.commit_assistant_stream(item_id)
    }

    pub(in crate::app) fn commit_assistant_stream(&mut self, item_id: Option<&str>) -> bool {
        if let Some(controller) = self.assistant_stream.take() {
            self.pending_tool_group = None;
            if let Some((lines, raw)) = controller.finalize_parts() {
                let id = self.next_item_id();
                self.push_history(TranscriptItem::assistant(id, lines, true, raw));
                self.committed_assistant_item_id = item_id.map(ToOwned::to_owned);
                self.committed_assistant_for_current_turn = true;
                self.set_active_assistant_lines(None);
                return true;
            }
        }
        self.set_active_assistant_lines(None);
        false
    }

    pub(in crate::app) fn finalize_reasoning_stream(&mut self) -> bool {
        let had_reasoning =
            self.reasoning_stream.is_some() || self.active_reasoning_lines.is_some();
        let in_flight_id = self.in_flight_reasoning_item_id.take();
        if let Some(controller) = self.reasoning_stream.take()
            && let Some((lines, raw)) = controller.finalize_parts()
        {
            let id = self.next_item_id();
            self.push_history(TranscriptItem::reasoning(id, lines, true, raw));
            self.set_active_reasoning_lines(None);
            if in_flight_id.is_some() {
                self.committed_reasoning_item_id = in_flight_id;
            }
            return true;
        }
        self.set_active_reasoning_lines(None);
        if had_reasoning {
            self.pending_tool_group = None;
        }
        false
    }

    pub(in crate::app) fn push_rendered_assistant_message(&mut self, message: &str) {
        let lines = self.render_markdown_lines(message);
        let id = self.next_item_id();
        self.push_history(TranscriptItem::assistant(
            id,
            lines,
            true,
            message.to_owned(),
        ));
    }

    pub(in crate::app) fn push_rendered_reasoning_message(&mut self, message: &str) {
        let lines = self.render_markdown_lines(message);
        let id = self.next_item_id();
        self.push_history(TranscriptItem::reasoning(
            id,
            lines,
            true,
            message.to_owned(),
        ));
    }

    pub(in crate::app) fn render_markdown_lines(&self, message: &str) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        crate::markdown::append_markdown(
            message,
            Some(usize::from(self.terminal_width.saturating_sub(6))),
            &mut lines,
        );
        lines
    }

    pub(in crate::app) fn push_history(&mut self, item: TranscriptItem) {
        self.pending_tool_group = None;
        self.transcript_items.push(item);
    }

    pub(in crate::app) fn push_info(&mut self, message: impl Into<String>) {
        let id = self.next_item_id();
        self.push_history(TranscriptItem::info(id, message));
    }

    pub(in crate::app) fn push_error(&mut self, message: impl Into<String>) {
        let id = self.next_item_id();
        self.push_history(TranscriptItem::error(id, message));
    }

    pub(in crate::app) fn push_tool_group_cell(&mut self, cell: ToolCallGroupCell) -> usize {
        let cell_idx = self.transcript_items.len();
        let tool_name = cell.tool_name().to_owned();
        let id = self.next_item_id();
        self.transcript_items
            .push(TranscriptItem::tool_group(id, cell));
        self.pending_tool_group = Some((cell_idx, tool_name));
        cell_idx
    }

    pub(in crate::app) fn tool_group_mut(&mut self, cell_idx: usize) -> &mut ToolCallGroupCell {
        match self
            .transcript_items
            .get_mut(cell_idx)
            .and_then(TranscriptItem::tool_group_mut)
        {
            Some(group) => group,
            None => panic!("tracked tool row should be a ToolCallGroupCell"),
        }
    }

    pub(in crate::app) fn mark_item_mutated(&mut self, cell_idx: usize) {
        if let Some(item) = self.transcript_items.get_mut(cell_idx) {
            item.mark_mutated();
        }
    }

    pub(in crate::app) fn replace_tool_call_with_file_changes(
        &mut self,
        call_id: &str,
        file_changes: Vec<FileChangeOutput>,
    ) -> bool {
        if file_changes.is_empty() {
            return false;
        }

        for file_change in &file_changes {
            self.recent_file_changes.push(file_change.clone());
        }
        while self.recent_file_changes.len() > 20 {
            self.recent_file_changes.remove(0);
        }

        let Some((cell_idx, entry_idx)) = self.tool_call_rows.remove(call_id) else {
            for file_change in file_changes {
                let id = self.next_item_id();
                self.push_history(TranscriptItem::patch(id, file_change));
            }
            return true;
        };

        let mut file_changes = file_changes.into_iter();
        let Some(first_change) = file_changes.next() else {
            return false;
        };

        if self.tool_group_mut(cell_idx).entry_count() == 1 {
            self.transcript_items[cell_idx].replace_with_patch(first_change);
            if self
                .pending_tool_group
                .as_ref()
                .is_some_and(|(idx, _)| *idx == cell_idx)
            {
                self.pending_tool_group = None;
            }
            for file_change in file_changes {
                let id = self.next_item_id();
                self.push_history(TranscriptItem::patch(id, file_change));
            }
            return true;
        }

        self.tool_group_mut(cell_idx)
            .set_entry_outcome(entry_idx, ToolCallState::Success, None);
        self.mark_item_mutated(cell_idx);
        let id = self.next_item_id();
        self.push_history(TranscriptItem::patch(id, first_change));
        for file_change in file_changes {
            let id = self.next_item_id();
            self.push_history(TranscriptItem::patch(id, file_change));
        }
        true
    }

    /// Replace a completed `todo_write` tool row with a checklist snapshot,
    /// mirroring `replace_tool_call_with_file_changes`.
    pub(in crate::app) fn replace_tool_call_with_todo_list(
        &mut self,
        call_id: &str,
        todos: Vec<TodoItem>,
    ) -> bool {
        if todos.is_empty() {
            return false;
        }

        let Some((cell_idx, entry_idx)) = self.tool_call_rows.remove(call_id) else {
            let id = self.next_item_id();
            self.push_history(TranscriptItem::todo_list(id, todos));
            return true;
        };

        if self.tool_group_mut(cell_idx).entry_count() == 1 {
            self.transcript_items[cell_idx].replace_with_todos(todos);
            if self
                .pending_tool_group
                .as_ref()
                .is_some_and(|(idx, _)| *idx == cell_idx)
            {
                self.pending_tool_group = None;
            }
            self.mark_item_mutated(cell_idx);
            return true;
        }

        self.tool_group_mut(cell_idx)
            .set_entry_outcome(entry_idx, ToolCallState::Success, None);
        self.mark_item_mutated(cell_idx);
        let id = self.next_item_id();
        self.push_history(TranscriptItem::todo_list(id, todos));
        true
    }

    pub(in crate::app) fn complete_subagent_tool_call(
        &mut self,
        child_thread_id: ThreadId,
        status: &AgentStatus,
    ) {
        let Some(call_id) = self.subagent_tool_calls.remove(&child_thread_id) else {
            return;
        };
        self.running_tools.remove(&call_id);
        let Some((cell_idx, entry_idx)) = self.tool_call_rows.remove(&call_id) else {
            return;
        };

        let (state, error) = match status {
            AgentStatus::Completed(_) => (ToolCallState::Success, None),
            AgentStatus::Errored(error) => (ToolCallState::Failure, Some(error.message.clone())),
            AgentStatus::Interrupted => (ToolCallState::Failure, Some(String::from("interrupted"))),
            AgentStatus::Shutdown => (ToolCallState::Failure, Some(String::from("shutdown"))),
            AgentStatus::NotFound => (ToolCallState::Failure, Some(String::from("not found"))),
            AgentStatus::PendingInit | AgentStatus::Running => (ToolCallState::Running, None),
        };
        self.tool_group_mut(cell_idx)
            .set_entry_outcome(entry_idx, state, error);
        self.mark_item_mutated(cell_idx);
    }

    pub(in crate::app) fn clear_transcript(&mut self) {
        self.exit_transcript_select();
        self.transcript_items.clear();
        self.set_active_assistant_lines(None);
        self.set_active_reasoning_lines(None);
        self.assistant_stream = None;
        self.reasoning_stream = None;
        self.tool_call_rows.clear();
        self.subagent_tool_calls.clear();
        self.pending_tool_group = None;
        self.running_tools.clear();
        self.recent_file_changes.clear();
        self.scroll = 0;
        self.auto_scroll = true;
        self.render_cache.clear();
        self.active_wrap = None;
    }

    pub(in crate::app) fn reset_turn_tracking(&mut self) {
        self.is_turn_running = false;
        self.is_turn_cancelling = false;
        self.current_turn_id = None;
        self.committed_assistant_item_id = None;
        self.committed_assistant_for_current_turn = false;
        self.committed_reasoning_item_id = None;
        self.in_flight_reasoning_item_id = None;
        self.plan_mode = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::*;

    #[test]
    fn stream_error_finalizes_partial_and_shows_reconnect_notice() {
        let mut app = App::new();

        start_turn(&mut app);
        app.handle_session_event(
            event(
                "1",
                EventMsg::AgentMessageDelta(AgentMessageDeltaEvent {
                    thread_id: String::from("thread"),
                    turn_id: String::from("turn-1"),
                    item_id: String::from("assistant-1"),
                    delta: String::from("partial"),
                }),
            ),
            20,
        );
        app.handle_session_event(
            event(
                "2",
                EventMsg::AgentMessageCompleted(AgentMessageCompletedEvent {
                    thread_id: String::from("thread"),
                    turn_id: String::from("turn-1"),
                    item_id: String::from("assistant-1"),
                    text: String::from("partial"),
                }),
            ),
            20,
        );
        app.handle_session_event(
            event(
                "3",
                EventMsg::StreamError(StreamErrorEvent {
                    thread_id: String::from("thread"),
                    turn_id: String::from("turn-1"),
                    message: String::from("Reconnecting… 1/8"),
                }),
            ),
            20,
        );
        app.handle_session_event(
            event(
                "4",
                EventMsg::AgentMessageDelta(AgentMessageDeltaEvent {
                    thread_id: String::from("thread"),
                    turn_id: String::from("turn-1"),
                    item_id: String::from("assistant-1#1"),
                    delta: String::from("continued"),
                }),
            ),
            20,
        );

        assert_eq!(
            transcript_strings(&app),
            vec![
                String::from("• partial"),
                String::new(),
                String::from("i Reconnecting… 1/8"),
                String::new(),
                String::from("• continued"),
            ]
        );
    }

    #[test]
    fn subagent_completion_finishes_status_update_tool_row()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut app = App::new();
        let child_thread_id = ThreadId::new();

        start_turn(&mut app);
        start_tool_call(
            &mut app,
            "2",
            "c1",
            "spawn_agent",
            "{\"description\":\"inspect\",\"prompt\":\"inspect\"}",
        );
        app.handle_session_event(
            event(
                "3",
                EventMsg::ToolCallCompleted(ToolCallCompletedEvent {
                    thread_id: String::from("thread"),
                    turn_id: String::from("turn-1"),
                    call_id: String::from("c1"),
                    success: true,
                    output_preview: Some(String::from("{\"status\":\"running\"}")),
                    error: None,
                    result_kind: ToolCallResultKind::StatusUpdate,
                    related_thread_id: Some(child_thread_id),
                    file_change: None,
                    file_changes: Vec::new(),
                    todos: Vec::new(),
                }),
            ),
            20,
        );
        app.handle_session_event(
            event(
                "4",
                EventMsg::CollabAgentCompleted(cazean_protocol::CollabAgentCompletedEvent {
                    parent_thread_id: ThreadId::new(),
                    child_thread_id,
                    agent_path: cazean_protocol::AgentPath::try_from("/root/child")?,
                    agent_nickname: Some("child".to_string()),
                    status: AgentStatus::Completed(Some("Done".to_string())),
                    last_assistant_message: Some("Done".to_string()),
                }),
            ),
            20,
        );

        let joined = transcript_strings(&app).join("\n");
        assert!(
            joined.contains("✓ spawn_agent {\"description\":\"inspect\",\"prompt\":\"inspect\"}")
        );
        assert!(
            !joined.contains("⠋ spawn_agent {\"description\":\"inspect\",\"prompt\":\"inspect\"}")
        );
        assert!(!joined.contains("Sub-agent"));
        assert!(!joined.contains("Done"));
        Ok(())
    }

    #[test]
    fn spawn_lifecycle_events_do_not_duplicate_tool_prompt() {
        let mut app = App::new();
        let prompt = "inspect protocol";

        start_turn(&mut app);
        start_tool_call(
            &mut app,
            "2",
            "c1",
            "spawn_agent",
            "{\"description\":\"protocol scan\",\"prompt\":\"inspect protocol\"}",
        );
        app.handle_session_event(
            event(
                "3",
                EventMsg::CollabAgentSpawnBegin(CollabAgentSpawnBeginEvent {
                    call_id: String::from("call"),
                    sender_thread_id: ThreadId::new(),
                    prompt: prompt.to_string(),
                    model: None,
                }),
            ),
            20,
        );
        app.handle_session_event(
            event(
                "4",
                EventMsg::CollabAgentSpawnEnd(CollabAgentSpawnEndEvent {
                    call_id: String::from("call"),
                    sender_thread_id: ThreadId::new(),
                    new_thread_id: Some(ThreadId::new()),
                    new_agent_nickname: Some(String::from("child")),
                    prompt: prompt.to_string(),
                    model: None,
                    status: AgentStatus::Running,
                }),
            ),
            20,
        );

        let joined = transcript_strings(&app).join("\n");
        assert!(joined.contains(
            "spawn_agent {\"description\":\"protocol scan\",\"prompt\":\"inspect protocol\"}"
        ));
        assert_eq!(joined.matches(prompt).count(), 1);
        assert!(!joined.contains("Spawning sub-agent"));
        assert!(!joined.contains("Spawn ended"));
    }

    #[test]
    fn resume_lifecycle_events_are_transcript_silent() {
        let mut app = App::new();
        let sender_thread_id = ThreadId::new();
        let receiver_thread_id = ThreadId::new();

        app.handle_session_event(
            event(
                "1",
                EventMsg::CollabResumeBegin(CollabResumeBeginEvent {
                    call_id: String::from("resume-child"),
                    sender_thread_id,
                    receiver_thread_id,
                    receiver_agent_nickname: Some(String::from("child")),
                }),
            ),
            20,
        );
        app.handle_session_event(
            event(
                "2",
                EventMsg::CollabResumeEnd(CollabResumeEndEvent {
                    call_id: String::from("resume-child"),
                    sender_thread_id,
                    receiver_thread_id,
                    receiver_agent_nickname: Some(String::from("child")),
                    status: AgentStatus::Completed(Some(String::from("done"))),
                }),
            ),
            20,
        );

        assert!(app.model.transcript_items.is_empty());
        let rendered = transcript_strings(&app).join("\n");
        assert!(!rendered.contains("Resuming agent"));
        assert!(!rendered.contains("Resume finished"));
    }

    #[test]
    fn protocol_event_from_inactive_thread_is_ignored() {
        let mut model = UiModel::new();
        let active_thread = ThreadId::new();
        let stale_thread = ThreadId::new();
        model.current_thread_id = Some(active_thread);
        model.screen = Screen::Dashboard;

        let effects = model.update(UiEvent::Protocol {
            source_thread_id: Some(stale_thread),
            event: Box::new(event(
                "stale",
                EventMsg::UserMessage {
                    text: "old prompt".to_string(),
                },
            )),
            viewport_height: 20,
        });

        assert!(effects.is_empty());
        assert_eq!(model.current_thread_id, Some(active_thread));
        assert_eq!(model.screen, Screen::Dashboard);
        assert!(model.transcript_items.is_empty());
    }

    #[test]
    fn tool_output_is_stored_on_completion() {
        let mut app = App::new();
        start_turn(&mut app);
        start_tool_call(&mut app, "1", "call-1", "run_command", "{}");
        complete_tool_call(&mut app, "2", "call-1", true, None);

        let stored = app
            .model
            .transcript_items
            .iter()
            .find_map(|item| item.tool_group_cell())
            .and_then(|cell| cell.copy_result());
        assert_eq!(stored.as_deref(), Some("BIG CONTENT"));
    }
}
