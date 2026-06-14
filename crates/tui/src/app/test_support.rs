use super::*;

pub(in crate::app) use app_server_protocol::{
    AskUserQuestion, AskUserQuestionOption, AskUserQuestionParams, PlanApprovalDecision,
    RequestPlanApprovalParams,
};
pub(in crate::app) use ratatui::{Terminal, backend::TestBackend};
pub(in crate::app) use smooth_protocol::{
    AgentMessageCompletedEvent, AgentMessageDeltaEvent, AgentReasoningCompletedEvent,
    AgentReasoningDeltaEvent, CollabAgentSpawnBeginEvent, CollabAgentSpawnEndEvent,
    CollabResumeBeginEvent, CollabResumeEndEvent, EventMsg, StreamErrorEvent,
    ToolCallCompletedEvent, ToolCallStartedEvent, TurnCompletedEvent, TurnInterruptedEvent,
    TurnStartedEvent,
};

pub(in crate::app) fn event(id: &str, msg: EventMsg) -> Event {
    Event {
        id: id.to_owned(),
        msg,
    }
}

pub(in crate::app) fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

pub(in crate::app) fn modified_key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, modifiers)
}

pub(in crate::app) fn workspace_insert_model() -> UiModel {
    let mut model = UiModel::new();
    model.screen = Screen::Workspace;
    model.mode = UiMode::Insert;
    model.focus = FocusTarget::Composer;
    model
}

pub(in crate::app) fn workspace_normal_model() -> UiModel {
    let mut model = UiModel::new();
    model.screen = Screen::Workspace;
    model.mode = UiMode::Normal;
    model.focus = FocusTarget::Transcript;
    model
}

pub(in crate::app) fn transcript_strings(app: &App) -> Vec<String> {
    app.transcript_lines(80)
        .into_iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect()
}

pub(in crate::app) fn rendered_buffer_text(terminal: &Terminal<TestBackend>) -> String {
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}

pub(in crate::app) fn buffer_rows(terminal: &Terminal<TestBackend>, width: usize) -> Vec<String> {
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<Vec<_>>()
        .chunks(width)
        .map(|row| row.concat())
        .collect()
}

pub(in crate::app) fn dashboard_thread(idx: usize) -> ThreadListItem {
    ThreadListItem {
        thread_id: format!("thread-{idx}"),
        rollout_path: format!("session-{idx}.jsonl"),
        created_at: "2026-05-31T00:00:00Z".to_string(),
        updated_at: format!("2026-05-31T00:{idx:02}:00Z"),
        last_user_message: Some(format!("message-{idx}")),
        last_assistant_message: None,
    }
}

pub(in crate::app) fn start_turn(app: &mut App) {
    app.handle_session_event(
        event(
            "turn-start",
            EventMsg::TurnStarted(TurnStartedEvent {
                thread_id: String::from("thread"),
                turn_id: String::from("turn-1"),
            }),
        ),
        20,
    );
}

pub(in crate::app) fn start_tool_call(
    app: &mut App,
    event_id: &str,
    call_id: &str,
    tool_name: &str,
    args: &str,
) {
    app.handle_session_event(
        event(
            event_id,
            EventMsg::ToolCallStarted(ToolCallStartedEvent {
                thread_id: String::from("thread"),
                turn_id: String::from("turn-1"),
                call_id: call_id.to_owned(),
                tool_name: tool_name.to_owned(),
                args_preview: args.to_owned(),
            }),
        ),
        20,
    );
}

pub(in crate::app) fn complete_tool_call(
    app: &mut App,
    event_id: &str,
    call_id: &str,
    success: bool,
    error: Option<&str>,
) {
    app.handle_session_event(
        event(
            event_id,
            EventMsg::ToolCallCompleted(ToolCallCompletedEvent {
                thread_id: String::from("thread"),
                turn_id: String::from("turn-1"),
                call_id: call_id.to_owned(),
                success,
                output_preview: Some(String::from("BIG CONTENT")),
                error: error.map(str::to_owned),
                result_kind: ToolCallResultKind::Final,
                related_thread_id: None,
                file_change: None,
                file_changes: Vec::new(),
                todos: Vec::new(),
            }),
        ),
        20,
    );
}

pub(in crate::app) fn complete_agent_message(
    app: &mut App,
    event_id: &str,
    item_id: &str,
    text: &str,
) {
    app.handle_session_event(
        event(
            event_id,
            EventMsg::AgentMessageCompleted(AgentMessageCompletedEvent {
                thread_id: String::from("thread"),
                turn_id: String::from("turn-1"),
                item_id: item_id.to_owned(),
                text: text.to_owned(),
            }),
        ),
        20,
    );
}

pub(in crate::app) fn reasoning_delta(app: &mut App, event_id: &str, item_id: &str, delta: &str) {
    app.handle_session_event(
        event(
            event_id,
            EventMsg::AgentReasoningDelta(AgentReasoningDeltaEvent {
                thread_id: String::from("thread"),
                turn_id: String::from("turn-1"),
                item_id: item_id.to_owned(),
                delta: delta.to_owned(),
            }),
        ),
        20,
    );
}

pub(in crate::app) fn complete_reasoning(app: &mut App, event_id: &str, item_id: &str, text: &str) {
    app.handle_session_event(
        event(
            event_id,
            EventMsg::AgentReasoningCompleted(AgentReasoningCompletedEvent {
                thread_id: String::from("thread"),
                turn_id: String::from("turn-1"),
                item_id: item_id.to_owned(),
                text: text.to_owned(),
            }),
        ),
        20,
    );
}

pub(in crate::app) fn skills_fixture()
-> Result<(tempfile::TempDir, UiModel), Box<dyn std::error::Error>> {
    let temp = tempfile::TempDir::new()?;
    for (name, description) in [("deploy", "Deploy the app"), ("review", "Review a PR")] {
        let dir = tools::skills_dir(temp.path()).join(name);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\ndescription: {description}\n---\nbody"),
        )?;
    }
    let mut model = workspace_insert_model();
    model.skills_root = temp.path().to_path_buf();
    Ok((temp, model))
}

pub(in crate::app) fn plan_approval_params(thread_id: &str) -> RequestPlanApprovalParams {
    RequestPlanApprovalParams {
        thread_id: thread_id.to_string(),
        turn_id: "turn-1".to_string(),
        call_id: "call-1".to_string(),
        plan: "# The plan\n\n1. Refactor the module.".to_string(),
    }
}

pub(in crate::app) fn select_model_with_items(count: usize) -> UiModel {
    let mut model = workspace_normal_model();
    for idx in 0..count {
        let id = model.next_item_id();
        model.push_history(TranscriptItem::info(id, format!("item {idx}")));
    }
    model
}

pub(in crate::app) fn enter_select(model: &mut UiModel, t0: Instant) {
    let _ = model.handle_key_event_at(key(KeyCode::Esc), t0);
    let _ = model.handle_key_event_at(key(KeyCode::Esc), t0 + Duration::from_millis(100));
}

pub(in crate::app) fn clipboard_content(effects: &[UiEffect]) -> Option<&str> {
    effects.iter().find_map(|effect| match &effect.kind {
        UiEffectKind::CopyToClipboard { content } => Some(content.as_str()),
        _ => None,
    })
}

pub(in crate::app) fn preview_response(
    thread_id: ThreadId,
    initial_messages: Vec<EventMsg>,
) -> ThreadPreviewResponse {
    ThreadPreviewResponse {
        thread_id: thread_id.to_string(),
        agent_path: Some("/root/worker".to_string()),
        agent_nickname: Some("worker".to_string()),
        status: AgentStatus::Running,
        is_live: true,
        initial_messages,
    }
}

pub(in crate::app) fn open_preview(
    model: &mut UiModel,
    thread_id: ThreadId,
    messages: Vec<EventMsg>,
) {
    let effect = model.effect(
        EffectContext::ThreadPreview { thread_id },
        UiEffectKind::ThreadPreview { thread_id },
    );
    let effects = model.update(UiEvent::EffectCompleted {
        effect_id: effect.effect_id,
        result: UiEffectResult::ThreadPreview(Box::new(preview_response(thread_id, messages))),
        viewport_height: 20,
    });
    assert!(effects.is_empty(), "a valid preview push emits no effects");
}

pub(in crate::app) fn unwatch_targets(effects: &[UiEffect]) -> Vec<ThreadId> {
    effects
        .iter()
        .filter_map(|effect| match effect.kind {
            UiEffectKind::ThreadUnwatch { thread_id } => Some(thread_id),
            _ => None,
        })
        .collect()
}

pub(in crate::app) fn preview_targets(effects: &[UiEffect]) -> Vec<ThreadId> {
    effects
        .iter()
        .filter_map(|effect| match effect.kind {
            UiEffectKind::ThreadPreview { thread_id } => Some(thread_id),
            _ => None,
        })
        .collect()
}

pub(in crate::app) fn child_event(id: &str, msg: EventMsg) -> Event {
    Event {
        id: id.to_owned(),
        msg,
    }
}

pub(in crate::app) trait JoinLines {
    fn join(self, separator: &str) -> String;
}

impl JoinLines for Vec<Line<'static>> {
    fn join(self, separator: &str) -> String {
        self.into_iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join(separator)
    }
}
