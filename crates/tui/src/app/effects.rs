use super::*;

impl UiModel {
    pub(in crate::app) fn apply_effect_result(
        &mut self,
        effect_id: EffectId,
        context: Option<EffectContext>,
        result: UiEffectResult,
    ) -> Vec<UiEffect> {
        match result {
            UiEffectResult::ThreadStart(response) => {
                let effects = self.clear_preview_stack();
                self.apply_thread_start_response(response);
                effects
            }
            UiEffectResult::ThreadList(response) => {
                self.dashboard.loading = false;
                self.dashboard.error = None;
                self.dashboard.next_cursor = response.next_cursor;
                self.dashboard.items = response.data;
                self.dashboard.selected = self
                    .dashboard
                    .selected
                    .min(self.dashboard.items.len().saturating_sub(1));
                self.dashboard_ensure_selected_visible(self.viewport_height);
                self.status_line = if self.dashboard.items.is_empty() {
                    String::from("No saved threads")
                } else {
                    format!("{} saved threads", self.dashboard.items.len())
                };
                Vec::new()
            }
            UiEffectResult::ThreadResume(response) => {
                let effects = self.clear_preview_stack();
                self.apply_thread_resume_response(effect_id, response);
                effects
            }
            UiEffectResult::TurnStart(response) => {
                if self.current_turn_id.as_deref() != Some(response.turn_id.as_str()) {
                    self.current_turn_id = Some(response.turn_id.clone());
                    self.is_turn_running = true;
                    self.is_turn_cancelling = false;
                    self.status_line = format!("Running turn {}", response.turn_id);
                }
                Vec::new()
            }
            UiEffectResult::TurnCancel(response) => {
                self.is_turn_cancelling = false;
                let cancelled_count = response.cancelled_thread_ids.len();
                self.status_line = if cancelled_count == 1 {
                    String::from("Cancel requested for 1 thread")
                } else {
                    format!("Cancel requested for {cancelled_count} threads")
                };
                Vec::new()
            }
            UiEffectResult::SetPlanMode(response) => {
                self.plan_mode = response.enabled;
                Vec::new()
            }
            UiEffectResult::ServerRequestAnswered => Vec::new(),
            UiEffectResult::ClipboardWritten => Vec::new(),
            UiEffectResult::ThreadPreview(response) => {
                self.apply_thread_preview(context, *response)
            }
            UiEffectResult::ThreadUnwatched => Vec::new(),
        }
    }

    pub(in crate::app) fn apply_effect_failure(
        &mut self,
        effect_id: EffectId,
        error: String,
    ) -> Vec<UiEffect> {
        let context = self.effect_contexts.remove(&effect_id);
        match context {
            Some(EffectContext::SetPlanMode { previous, desired }) => {
                self.plan_mode = previous;
                self.push_error(format!(
                    "could not {} plan mode: {error}",
                    if desired { "enable" } else { "disable" }
                ));
                Vec::new()
            }
            Some(EffectContext::ThreadList) => {
                self.dashboard.loading = false;
                self.dashboard.error = Some(error.clone());
                self.status_line = String::from("Could not list threads");
                Vec::new()
            }
            Some(EffectContext::TurnStart { thread_id, input }) => {
                self.is_turn_running = false;
                self.is_turn_cancelling = false;
                if self.composer.is_empty() {
                    self.composer.set_text(input);
                    self.mode = UiMode::Insert;
                    self.focus = FocusTarget::Composer;
                }
                self.push_error(format!("could not start turn on {thread_id}: {error}"));
                Vec::new()
            }
            Some(EffectContext::TurnCancel { thread_id }) => {
                self.is_turn_cancelling = false;
                self.status_line = self
                    .current_turn_id
                    .as_deref()
                    .map(|turn_id| format!("Running turn {turn_id}"))
                    .unwrap_or_else(|| String::from("Running turn"));
                self.push_error(format!("could not cancel turn on {thread_id}: {error}"));
                Vec::new()
            }
            Some(EffectContext::ThreadStart) => {
                self.dashboard.loading = false;
                self.dashboard.error = Some(format!("could not start thread: {error}"));
                self.status_line = String::from("Could not start thread");
                self.push_error(format!("could not start thread: {error}"));
                Vec::new()
            }
            Some(EffectContext::ThreadResume { thread_id }) => {
                self.dashboard.loading = false;
                self.dashboard.error =
                    Some(format!("could not resume thread {thread_id}: {error}"));
                self.status_line = String::from("Could not resume thread");
                self.push_error(format!("could not resume thread {thread_id}: {error}"));
                Vec::new()
            }
            Some(EffectContext::ServerRequest) => {
                self.push_error(format!("could not answer server request: {error}"));
                Vec::new()
            }
            Some(EffectContext::ThreadPreview { thread_id }) => {
                self.status_line = format!("Could not open subagent {thread_id}");
                self.push_error(format!("could not open subagent {thread_id}: {error}"));
                // The server may have taken a preview watcher before the
                // failure surfaced client-side; no view was pushed, so no pop
                // will release it. Unwatching with no watcher is a no-op.
                vec![self.effect(
                    EffectContext::ThreadUnwatch,
                    UiEffectKind::ThreadUnwatch { thread_id },
                )]
            }
            // Releasing a watcher is best-effort; a stale server-side
            // subscription only forwards events the TUI will drop.
            Some(EffectContext::ThreadUnwatch) => Vec::new(),
            Some(EffectContext::Clipboard) => {
                self.status_line = String::from("Copy failed");
                self.push_error(format!("could not copy to clipboard: {error}"));
                Vec::new()
            }
            Some(EffectContext::Exit) | None => Vec::new(),
        }
    }

    pub(in crate::app) fn apply_thread_start_response(&mut self, response: ThreadStartResponse) {
        match response.thread_id.parse::<ThreadId>() {
            Ok(thread_id) => {
                self.current_thread_id = Some(thread_id);
                self.screen = Screen::Workspace;
                self.mode = UiMode::Insert;
                self.focus = FocusTarget::Composer;
                self.status_line = format!("Thread {}", response.thread_id);
                self.reset_turn_tracking();
                self.clear_transcript();
            }
            Err(err) => {
                self.push_error(format!("Invalid started thread id: {err}"));
            }
        }
    }

    pub(in crate::app) fn apply_thread_resume_response(
        &mut self,
        effect_id: EffectId,
        response: ThreadResumeResponse,
    ) {
        match response.thread_id.parse::<ThreadId>() {
            Ok(thread_id) => {
                self.current_thread_id = Some(thread_id);
                self.screen = Screen::Workspace;
                self.mode = UiMode::Normal;
                self.focus = FocusTarget::Transcript;
                self.status_line = format!("Resumed thread {}", response.thread_id);
                self.reset_turn_tracking();
                self.clear_transcript();
                for (idx, msg) in response.initial_messages.into_iter().enumerate() {
                    self.apply_protocol_event(Event {
                        id: format!("resume-{}-{idx}", effect_id.0),
                        msg,
                    });
                }
            }
            Err(err) => {
                self.push_error(format!("Invalid resumed thread id: {err}"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::*;

    #[test]
    fn replaying_initial_messages_reconstructs_without_active_streams() {
        let mut model = UiModel::new();
        let thread_id = ThreadId::new();
        let _ = model.update(UiEvent::EffectCompleted {
            effect_id: EffectId(7),
            result: UiEffectResult::ThreadResume(ThreadResumeResponse {
                thread_id: thread_id.to_string(),
                rollout_path: "session.jsonl".to_string(),
                initial_messages: vec![
                    EventMsg::UserMessage {
                        text: "hello".to_string(),
                    },
                    EventMsg::AgentReasoningCompleted(AgentReasoningCompletedEvent {
                        thread_id: thread_id.to_string(),
                        turn_id: "turn".to_string(),
                        item_id: "r1".to_string(),
                        text: "thinking".to_string(),
                    }),
                    EventMsg::AgentMessageCompleted(AgentMessageCompletedEvent {
                        thread_id: thread_id.to_string(),
                        turn_id: "turn".to_string(),
                        item_id: "a1".to_string(),
                        text: "world".to_string(),
                    }),
                ],
            }),
            viewport_height: 20,
        });

        let joined = model
            .transcript_lines_uncached(80)
            .into_iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(joined.contains("▌ hello"));
        assert!(joined.contains("thinking"));
        assert!(joined.contains("• world"));
        assert!(model.active_assistant_lines.is_none());
        assert!(model.active_reasoning_lines.is_none());
    }
}
