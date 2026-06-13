use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use app_server_protocol::{
    AskUserQuestionResponse, JsonRpcError, RequestId, RequestPlanApprovalResponse, ServerRequest,
    SetPlanModeResponse, ThreadListItem, ThreadListResponse, ThreadPreviewResponse,
    ThreadResumeResponse, ThreadStartResponse, TurnCancelResponse, TurnStartResponse,
};
use crossterm::event::{Event as CrosstermEvent, KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::Paragraph,
};
use smooth_protocol::{
    AgentStatus, ErrorInfo, Event, EventMsg, FileChangeOutput, ThreadId, TodoItem,
    ToolCallResultKind,
};

use crate::{
    AppTerminal,
    app_server_session::AppServerSession,
    composer::ComposerState,
    diff_render::file_change_path_label,
    error::TuiResult,
    history_cell::{ToolCallGroupCell, ToolCallState, TranscriptItem, TranscriptItemId},
    plan_approval::{PlanApprovalOutcome, PlanApprovalOverlay},
    question_picker::{PickerOutcome, QuestionPicker},
    skill_popup::SkillPopup,
    streaming::StreamController,
    subagent_preview::SubagentPreviewView,
    wrap,
};

#[derive(Debug)]
pub(crate) enum AppRunControl {
    Continue,
    Exit,
}

/// Window within which a second Esc press enters transcript-select mode.
const DOUBLE_ESC_WINDOW: Duration = Duration::from_millis(500);
/// Window within which a second `y` press upgrades a tool-result copy to args.
const COPY_CHORD_WINDOW: Duration = Duration::from_millis(500);
/// Window within which a `g` prefix completes as `gg` (top) or `gd` (subagent).
const GOTO_CHORD_WINDOW: Duration = Duration::from_millis(500);
/// Cap on the OSC 52 payload; terminals commonly reject larger sequences.
const MAX_CLIPBOARD_BYTES: usize = 100_000;

pub(crate) struct App {
    model: UiModel,
}

impl App {
    pub(crate) fn new() -> Self {
        Self {
            model: UiModel::new(),
        }
    }

    pub(crate) async fn startup(
        &mut self,
        app_server: &mut AppServerSession,
        viewport_height: u16,
    ) -> TuiResult<AppRunControl> {
        let effects = self.model.update(UiEvent::Startup { viewport_height });
        self.run_effects(app_server, effects, viewport_height).await
    }

    pub(crate) fn handle_session_event_from_thread(
        &mut self,
        source_thread_id: ThreadId,
        event: Event,
        viewport_height: u16,
    ) {
        let effects = self.model.update(UiEvent::Protocol {
            source_thread_id: Some(source_thread_id),
            event: Box::new(event),
            viewport_height,
        });
        debug_assert!(effects.is_empty());
    }

    #[cfg(test)]
    fn handle_session_event(&mut self, event: Event, viewport_height: u16) {
        let effects = self.model.update(UiEvent::Protocol {
            source_thread_id: None,
            event: Box::new(event),
            viewport_height,
        });
        debug_assert!(effects.is_empty());
    }

    pub(crate) async fn handle_terminal_event(
        &mut self,
        app_server: &mut AppServerSession,
        event: CrosstermEvent,
        viewport_height: u16,
    ) -> TuiResult<AppRunControl> {
        let effects = self.model.update(UiEvent::Terminal {
            event,
            viewport_height,
        });
        self.run_effects(app_server, effects, viewport_height).await
    }

    pub(crate) async fn handle_server_request(
        &mut self,
        app_server: &mut AppServerSession,
        request: ServerRequest,
        viewport_height: u16,
    ) -> TuiResult<AppRunControl> {
        let effects = self.model.update(UiEvent::ServerRequest(request));
        self.run_effects(app_server, effects, viewport_height).await
    }

    async fn run_effects(
        &mut self,
        app_server: &mut AppServerSession,
        effects: Vec<UiEffect>,
        viewport_height: u16,
    ) -> TuiResult<AppRunControl> {
        let mut queue = VecDeque::from(effects);
        while let Some(effect) = queue.pop_front() {
            let effect_id = effect.effect_id;
            let result = match effect.kind {
                UiEffectKind::Exit => return Ok(AppRunControl::Exit),
                UiEffectKind::ThreadStart => app_server
                    .start_thread()
                    .await
                    .map(UiEffectResult::ThreadStart),
                UiEffectKind::ThreadList { cursor, limit } => app_server
                    .thread_list(cursor, limit)
                    .await
                    .map(UiEffectResult::ThreadList),
                UiEffectKind::ThreadResume { thread_id } => app_server
                    .thread_resume(thread_id)
                    .await
                    .map(UiEffectResult::ThreadResume),
                UiEffectKind::ThreadPreview { thread_id } => app_server
                    .thread_preview(thread_id)
                    .await
                    .map(|response| UiEffectResult::ThreadPreview(Box::new(response))),
                UiEffectKind::ThreadUnwatch { thread_id } => app_server
                    .thread_unwatch(thread_id)
                    .await
                    .map(|_| UiEffectResult::ThreadUnwatched),
                UiEffectKind::TurnStart { thread_id, input } => app_server
                    .turn_start(thread_id, input)
                    .await
                    .map(UiEffectResult::TurnStart),
                UiEffectKind::TurnCancel { thread_id } => app_server
                    .turn_cancel(thread_id)
                    .await
                    .map(UiEffectResult::TurnCancel),
                UiEffectKind::SetPlanMode { thread_id, enabled } => app_server
                    .set_plan_mode(thread_id, enabled)
                    .await
                    .map(UiEffectResult::SetPlanMode),
                UiEffectKind::AnswerQuestion {
                    request_id,
                    response,
                } => {
                    let value = serde_json::to_value(response)?;
                    app_server
                        .respond_to_server_request(request_id, value)
                        .await
                        .map(|_| UiEffectResult::ServerRequestAnswered)
                }
                UiEffectKind::RespondPlanApproval {
                    request_id,
                    response,
                } => {
                    let value = serde_json::to_value(response)?;
                    app_server
                        .respond_to_server_request(request_id, value)
                        .await
                        .map(|_| UiEffectResult::ServerRequestAnswered)
                }
                UiEffectKind::FailQuestion { request_id, error } => app_server
                    .fail_server_request(request_id, error)
                    .await
                    .map(|_| UiEffectResult::ServerRequestAnswered),
                UiEffectKind::FailServerRequest { request_id, error } => app_server
                    .fail_server_request(request_id, error)
                    .await
                    .map(|_| UiEffectResult::ServerRequestAnswered),
                UiEffectKind::CopyToClipboard { content } => {
                    write_clipboard_osc52(&content).map(|()| UiEffectResult::ClipboardWritten)
                }
            };

            let next = match result {
                Ok(result) => self.model.update(UiEvent::EffectCompleted {
                    effect_id,
                    result,
                    viewport_height,
                }),
                Err(err) => self.model.update(UiEvent::EffectFailed {
                    effect_id,
                    error: err.to_string(),
                    viewport_height,
                }),
            };
            queue.extend(next);
        }

        Ok(AppRunControl::Continue)
    }

    pub(crate) fn render(&mut self, frame: &mut Frame<'_>) {
        self.model.render(frame);
    }

    pub(crate) fn viewport_height_for(&self, terminal: &AppTerminal) -> TuiResult<u16> {
        let size = terminal.size()?;
        Ok(self
            .model
            .transcript_viewport_height(size.width, size.height))
    }

    #[cfg(test)]
    fn transcript_lines(&self, width: u16) -> Vec<Line<'static>> {
        self.model.transcript_lines_uncached(width)
    }

    #[cfg(test)]
    fn max_scroll(&mut self, viewport_height: u16) -> u16 {
        self.model.max_scroll(viewport_height)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct EffectId(u64);

#[derive(Debug, Clone)]
struct UiEffect {
    effect_id: EffectId,
    kind: UiEffectKind,
}

#[derive(Debug, Clone)]
enum UiEffectKind {
    ThreadStart,
    ThreadList {
        cursor: Option<String>,
        limit: Option<u32>,
    },
    ThreadResume {
        thread_id: ThreadId,
    },
    ThreadPreview {
        thread_id: ThreadId,
    },
    ThreadUnwatch {
        thread_id: ThreadId,
    },
    TurnStart {
        thread_id: ThreadId,
        input: String,
    },
    TurnCancel {
        thread_id: ThreadId,
    },
    SetPlanMode {
        thread_id: ThreadId,
        enabled: bool,
    },
    AnswerQuestion {
        request_id: RequestId,
        response: AskUserQuestionResponse,
    },
    RespondPlanApproval {
        request_id: RequestId,
        response: RequestPlanApprovalResponse,
    },
    FailQuestion {
        request_id: RequestId,
        error: JsonRpcError,
    },
    FailServerRequest {
        request_id: RequestId,
        error: JsonRpcError,
    },
    CopyToClipboard {
        content: String,
    },
    Exit,
}

#[derive(Debug, Clone)]
enum UiEffectResult {
    ThreadStart(ThreadStartResponse),
    ThreadList(ThreadListResponse),
    ThreadResume(ThreadResumeResponse),
    ThreadPreview(Box<ThreadPreviewResponse>),
    ThreadUnwatched,
    TurnStart(TurnStartResponse),
    TurnCancel(TurnCancelResponse),
    SetPlanMode(SetPlanModeResponse),
    ServerRequestAnswered,
    ClipboardWritten,
}

#[derive(Debug, Clone)]
enum EffectContext {
    ThreadStart,
    ThreadList,
    ThreadResume { thread_id: ThreadId },
    ThreadPreview { thread_id: ThreadId },
    ThreadUnwatch,
    TurnStart { thread_id: ThreadId, input: String },
    TurnCancel { thread_id: ThreadId },
    SetPlanMode { previous: bool, desired: bool },
    ServerRequest,
    Clipboard,
    Exit,
}

#[derive(Debug)]
enum UiEvent {
    Startup {
        viewport_height: u16,
    },
    Terminal {
        event: CrosstermEvent,
        viewport_height: u16,
    },
    Protocol {
        source_thread_id: Option<ThreadId>,
        event: Box<Event>,
        viewport_height: u16,
    },
    ServerRequest(ServerRequest),
    EffectCompleted {
        effect_id: EffectId,
        result: UiEffectResult,
        viewport_height: u16,
    },
    EffectFailed {
        effect_id: EffectId,
        error: String,
        viewport_height: u16,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Screen {
    Dashboard,
    Workspace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UiMode {
    Normal,
    Insert,
    Command,
    Overlay,
    TranscriptSelect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FocusTarget {
    Dashboard,
    Transcript,
    Inspector,
    Composer,
    Overlay,
}

#[derive(Debug, Default)]
struct DashboardState {
    items: Vec<ThreadListItem>,
    selected: usize,
    scroll_offset: usize,
    loading: bool,
    error: Option<String>,
    next_cursor: Option<String>,
}

#[derive(Debug, Clone)]
struct RunningToolInfo {
    tool_name: String,
    args_preview: String,
}

/// State for transcript-select mode: which row is selected and, after a `y` on
/// a tool row, the pending chance to upgrade the copy to the tool args.
#[derive(Debug, Clone, Copy)]
struct TranscriptSelectState {
    selected: usize,
    pending_args: Option<(usize, Instant)>,
    /// Armed by `g`: a second `g` within the chord window jumps to the top
    /// (`gg`), a `d` opens the selected row's subagent session (`gd`).
    pending_g: Option<Instant>,
}

struct UiModel {
    current_thread_id: Option<ThreadId>,
    transcript_items: Vec<TranscriptItem>,
    next_transcript_item_id: TranscriptItemId,
    active_assistant_lines: Option<Vec<Line<'static>>>,
    active_reasoning_lines: Option<Vec<Line<'static>>>,
    assistant_stream: Option<StreamController>,
    reasoning_stream: Option<StreamController>,
    tool_call_rows: HashMap<String, (usize, usize)>,
    subagent_tool_calls: HashMap<ThreadId, String>,
    pending_tool_group: Option<(usize, String)>,
    current_turn_id: Option<String>,
    committed_assistant_item_id: Option<String>,
    committed_assistant_for_current_turn: bool,
    committed_reasoning_item_id: Option<String>,
    in_flight_reasoning_item_id: Option<String>,
    composer: ComposerState,
    command: String,
    status_line: String,
    scroll: u16,
    auto_scroll: bool,
    is_turn_running: bool,
    is_turn_cancelling: bool,
    plan_mode: bool,
    inspector_visible: bool,
    terminal_width: u16,
    viewport_height: u16,
    // Inner width of the transcript pane as last drawn by `render_transcript`.
    // Row counting must use this, not `terminal_width`: in the split workspace
    // the transcript only gets ~70% of the width, so wrapping (and therefore the
    // row count that drives `max_scroll`) differs from the full terminal width.
    transcript_inner_width: u16,
    question_picker: Option<QuestionPicker>,
    plan_approval: Option<PlanApprovalOverlay>,
    skill_popup: Option<SkillPopup>,
    /// Root directory skills are discovered from when the composer holds a
    /// leading `/token`; the process cwd outside of tests.
    skills_root: std::path::PathBuf,
    effect_counter: u64,
    effect_contexts: HashMap<EffectId, EffectContext>,
    screen: Screen,
    mode: UiMode,
    focus: FocusTarget,
    dashboard: DashboardState,
    transcript_select: Option<TranscriptSelectState>,
    /// Stacked full-screen subagent previews (`gd`); the top view receives
    /// keys and is rendered, nesting pushes deeper.
    preview_stack: Vec<SubagentPreviewView>,
    /// Timestamp of the last Esc press in Normal mode (or leaving Insert),
    /// armed for double-Esc detection.
    last_esc: Option<Instant>,
    running_tools: HashMap<String, RunningToolInfo>,
    recent_file_changes: Vec<FileChangeOutput>,
    render_cache: RenderedTranscriptCache,
    // Active (in-flight) assistant/reasoning streams are not in `render_cache`
    // (they mutate every delta). `active_version` bumps on every change to the
    // active lines so the wrapped result can be cached by (width, version) and
    // reused across row counting and visible-line construction within a frame.
    active_version: u64,
    active_wrap: Option<ActiveWrap>,
    /// Test-only: counts how many times the active wrap was actually recomputed
    /// (cache misses), so a test can assert the `(width, version)` memo holds.
    #[cfg(test)]
    active_wrap_computes: usize,
}

impl UiModel {
    fn new() -> Self {
        Self {
            current_thread_id: None,
            transcript_items: Vec::new(),
            next_transcript_item_id: 1,
            active_assistant_lines: None,
            active_reasoning_lines: None,
            assistant_stream: None,
            reasoning_stream: None,
            tool_call_rows: HashMap::new(),
            subagent_tool_calls: HashMap::new(),
            pending_tool_group: None,
            current_turn_id: None,
            committed_assistant_item_id: None,
            committed_assistant_for_current_turn: false,
            committed_reasoning_item_id: None,
            in_flight_reasoning_item_id: None,
            composer: ComposerState::default(),
            command: String::new(),
            status_line: String::from("Idle"),
            scroll: 0,
            auto_scroll: true,
            is_turn_running: false,
            is_turn_cancelling: false,
            plan_mode: false,
            inspector_visible: true,
            terminal_width: 80,
            viewport_height: 20,
            transcript_inner_width: 78,
            question_picker: None,
            plan_approval: None,
            skill_popup: None,
            skills_root: std::env::current_dir().unwrap_or_default(),
            effect_counter: 0,
            effect_contexts: HashMap::new(),
            screen: Screen::Dashboard,
            mode: UiMode::Normal,
            focus: FocusTarget::Dashboard,
            dashboard: DashboardState::default(),
            transcript_select: None,
            preview_stack: Vec::new(),
            last_esc: None,
            running_tools: HashMap::new(),
            recent_file_changes: Vec::new(),
            render_cache: RenderedTranscriptCache::default(),
            active_version: 0,
            active_wrap: None,
            #[cfg(test)]
            active_wrap_computes: 0,
        }
    }

    fn update(&mut self, event: UiEvent) -> Vec<UiEffect> {
        match event {
            UiEvent::Startup { viewport_height } => {
                self.viewport_height = viewport_height;
                self.dashboard.loading = true;
                vec![self.effect(
                    EffectContext::ThreadList,
                    UiEffectKind::ThreadList {
                        cursor: None,
                        limit: Some(50),
                    },
                )]
            }
            UiEvent::Terminal {
                event,
                viewport_height,
            } => {
                self.viewport_height = viewport_height;
                self.handle_terminal_event(event)
            }
            UiEvent::Protocol {
                source_thread_id,
                event,
                viewport_height,
            } => {
                self.viewport_height = viewport_height;
                // A parent's CollabAgentCompleted names a previewed child by
                // id, not by source thread: patch every matching view first,
                // before source routing can consume the event (with a nested
                // stack [A, B], B's completion arrives with source A).
                if let EventMsg::CollabAgentCompleted(completed) = &event.msg {
                    for view in &mut self.preview_stack {
                        if view.thread_id == completed.child_thread_id {
                            view.complete_from_parent(completed.status.clone());
                        }
                    }
                }
                // Events from previewed threads feed their stack views and
                // never touch the main transcript.
                if let Some(source) = source_thread_id
                    && self
                        .preview_stack
                        .iter()
                        .any(|view| view.thread_id == source)
                {
                    // Previews render full-screen, so wrap at terminal width.
                    let width = self.terminal_width.max(1);
                    for view in &mut self.preview_stack {
                        if view.thread_id != source {
                            continue;
                        }
                        view.apply_event(event.msg.clone(), width);
                        if view.auto_scroll {
                            view.scroll_to_bottom(width, viewport_height);
                        }
                    }
                    return Vec::new();
                }
                if !self.should_apply_protocol_event(source_thread_id) {
                    return Vec::new();
                }
                self.screen = Screen::Workspace;
                self.apply_protocol_event(*event);
                if self.auto_scroll {
                    self.scroll_to_bottom(viewport_height);
                }
                Vec::new()
            }
            UiEvent::ServerRequest(request) => self.handle_server_request(request),
            UiEvent::EffectCompleted {
                effect_id,
                result,
                viewport_height,
            } => {
                self.viewport_height = viewport_height;
                let context = self.effect_contexts.remove(&effect_id);
                let effects = self.apply_effect_result(effect_id, context, result);
                if self.auto_scroll {
                    self.scroll_to_bottom(viewport_height);
                }
                effects
            }
            UiEvent::EffectFailed {
                effect_id,
                error,
                viewport_height,
            } => {
                self.viewport_height = viewport_height;
                let effects = self.apply_effect_failure(effect_id, error);
                if self.auto_scroll {
                    self.scroll_to_bottom(viewport_height);
                }
                effects
            }
        }
    }

    fn should_apply_protocol_event(&self, source_thread_id: Option<ThreadId>) -> bool {
        match (self.current_thread_id, source_thread_id) {
            (Some(current), Some(source)) => current == source,
            _ => true,
        }
    }

    fn effect(&mut self, context: EffectContext, kind: UiEffectKind) -> UiEffect {
        self.effect_counter = self.effect_counter.saturating_add(1);
        let effect_id = EffectId(self.effect_counter);
        self.effect_contexts.insert(effect_id, context);
        UiEffect { effect_id, kind }
    }

    fn next_item_id(&mut self) -> TranscriptItemId {
        let id = self.next_transcript_item_id;
        self.next_transcript_item_id = self.next_transcript_item_id.saturating_add(1);
        id
    }

    fn handle_terminal_event(&mut self, event: CrosstermEvent) -> Vec<UiEffect> {
        match event {
            CrosstermEvent::Key(key_event) => self.handle_key_event(key_event),
            CrosstermEvent::Paste(text) => self.handle_paste_event(text),
            CrosstermEvent::Resize(width, height) => {
                self.terminal_width = width;
                self.viewport_height = self.transcript_viewport_height(width, height);
                self.render_cache
                    .evict_stale_widths(self.transcript_cache_width_hint(width));
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn handle_paste_event(&mut self, text: String) -> Vec<UiEffect> {
        if self.screen == Screen::Workspace {
            if let Some(picker) = self.question_picker.as_mut() {
                picker.handle_paste(&text);
            } else if self.mode == UiMode::Insert {
                self.composer.insert_paste(&text);
                self.sync_skill_popup();
            }
        }
        Vec::new()
    }

    fn handle_key_event(&mut self, key_event: KeyEvent) -> Vec<UiEffect> {
        self.handle_key_event_at(key_event, Instant::now())
    }

    fn handle_key_event_at(&mut self, key_event: KeyEvent, now: Instant) -> Vec<UiEffect> {
        if key_event.kind != crossterm::event::KeyEventKind::Press {
            return Vec::new();
        }

        // Any non-Esc key breaks the double-Esc chord.
        if key_event.code != KeyCode::Esc {
            self.last_esc = None;
        }

        if matches!(key_event.code, KeyCode::Char('c'))
            && key_event.modifiers.contains(KeyModifiers::CONTROL)
        {
            if self.is_turn_running {
                return self.request_turn_cancel();
            }
            return vec![self.effect(EffectContext::Exit, UiEffectKind::Exit)];
        }

        // A stacked subagent preview owns the keyboard (server-driven
        // overlays clear the stack on arrival, so they cannot coexist).
        if self.screen == Screen::Workspace && !self.preview_stack.is_empty() {
            return self.handle_preview_key(key_event, now);
        }

        if self.screen == Screen::Workspace && self.question_picker.is_some() {
            return self.dispatch_picker_key(key_event);
        }

        if self.screen == Screen::Workspace && self.plan_approval.is_some() {
            return self.dispatch_plan_approval_key(key_event);
        }

        if self.mode == UiMode::Command {
            return self.handle_command_key(key_event);
        }

        match self.screen {
            Screen::Dashboard => self.handle_dashboard_key(key_event),
            Screen::Workspace => self.handle_workspace_key(key_event, now),
        }
    }

    fn handle_dashboard_key(&mut self, key_event: KeyEvent) -> Vec<UiEffect> {
        match key_event.code {
            KeyCode::Char('n') => {
                self.status_line = String::from("Starting new thread");
                vec![self.effect(EffectContext::ThreadStart, UiEffectKind::ThreadStart)]
            }
            KeyCode::Enter => {
                let Some(item) = self.dashboard.items.get(self.dashboard.selected) else {
                    return Vec::new();
                };
                match item.thread_id.parse::<ThreadId>() {
                    Ok(thread_id) => {
                        self.status_line = format!("Resuming thread {}", item.thread_id);
                        vec![self.effect(
                            EffectContext::ThreadResume { thread_id },
                            UiEffectKind::ThreadResume { thread_id },
                        )]
                    }
                    Err(err) => {
                        self.dashboard.error = Some(format!("invalid thread id: {err}"));
                        Vec::new()
                    }
                }
            }
            KeyCode::Char(':') => {
                self.mode = UiMode::Command;
                self.command.clear();
                Vec::new()
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.dashboard.selected = self.dashboard.selected.saturating_sub(1);
                self.dashboard_ensure_selected_visible(self.viewport_height);
                Vec::new()
            }
            KeyCode::Down | KeyCode::Char('j') => {
                let max = self.dashboard.items.len().saturating_sub(1);
                self.dashboard.selected = self.dashboard.selected.saturating_add(1).min(max);
                self.dashboard_ensure_selected_visible(self.viewport_height);
                Vec::new()
            }
            KeyCode::PageUp => {
                let amount = self
                    .dashboard_visible_item_count(self.viewport_height)
                    .max(1);
                self.dashboard.selected = self.dashboard.selected.saturating_sub(amount);
                self.dashboard_ensure_selected_visible(self.viewport_height);
                Vec::new()
            }
            KeyCode::PageDown => {
                let amount = self
                    .dashboard_visible_item_count(self.viewport_height)
                    .max(1);
                let max = self.dashboard.items.len().saturating_sub(1);
                self.dashboard.selected = self.dashboard.selected.saturating_add(amount).min(max);
                self.dashboard_ensure_selected_visible(self.viewport_height);
                Vec::new()
            }
            KeyCode::Home => {
                self.dashboard.selected = 0;
                self.dashboard_ensure_selected_visible(self.viewport_height);
                Vec::new()
            }
            KeyCode::End => {
                self.dashboard.selected = self.dashboard.items.len().saturating_sub(1);
                self.dashboard_ensure_selected_visible(self.viewport_height);
                Vec::new()
            }
            KeyCode::Char('q') | KeyCode::Esc => {
                vec![self.effect(EffectContext::Exit, UiEffectKind::Exit)]
            }
            _ => Vec::new(),
        }
    }

    fn handle_workspace_key(&mut self, key_event: KeyEvent, now: Instant) -> Vec<UiEffect> {
        match self.mode {
            UiMode::Normal => self.handle_normal_key(key_event, now),
            UiMode::Insert => self.handle_insert_key(key_event, now),
            UiMode::TranscriptSelect => self.handle_transcript_select_key(key_event, now),
            UiMode::Command | UiMode::Overlay => Vec::new(),
        }
    }

    fn handle_normal_key(&mut self, key_event: KeyEvent, now: Instant) -> Vec<UiEffect> {
        match key_event.code {
            KeyCode::Char('i') => {
                self.mode = UiMode::Insert;
                self.focus = FocusTarget::Composer;
                Vec::new()
            }
            KeyCode::Char('I') => {
                self.toggle_inspector_visible();
                Vec::new()
            }
            KeyCode::Char(':') => {
                self.mode = UiMode::Command;
                self.command.clear();
                Vec::new()
            }
            KeyCode::Char('q') if self.composer.is_empty() => {
                vec![self.effect(EffectContext::Exit, UiEffectKind::Exit)]
            }
            KeyCode::Tab => {
                self.focus_next();
                Vec::new()
            }
            KeyCode::BackTab => {
                self.focus_prev();
                Vec::new()
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.scroll_up(1);
                Vec::new()
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.scroll_down(1, self.viewport_height);
                Vec::new()
            }
            KeyCode::PageUp => {
                self.scroll_up(self.viewport_height.saturating_sub(1).max(1));
                Vec::new()
            }
            KeyCode::PageDown => {
                self.scroll_down(
                    self.viewport_height.saturating_sub(1).max(1),
                    self.viewport_height,
                );
                Vec::new()
            }
            KeyCode::Home => {
                self.scroll = 0;
                self.auto_scroll = false;
                Vec::new()
            }
            KeyCode::End => {
                self.auto_scroll = true;
                self.scroll_to_bottom(self.viewport_height);
                Vec::new()
            }
            KeyCode::Esc => {
                if self
                    .last_esc
                    .is_some_and(|t| now.duration_since(t) <= DOUBLE_ESC_WINDOW)
                {
                    self.last_esc = None;
                    self.enter_transcript_select();
                } else {
                    self.last_esc = Some(now);
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// Enter transcript-select mode (double-Esc). Selection starts on the last
    /// transcript row; follow-mode is suspended so streaming output doesn't
    /// yank the view while the user navigates.
    fn enter_transcript_select(&mut self) {
        if self.screen != Screen::Workspace || self.mode != UiMode::Normal {
            return;
        }
        if self.transcript_items.is_empty() {
            self.status_line = String::from("Transcript is empty");
            return;
        }
        self.mode = UiMode::TranscriptSelect;
        self.focus = FocusTarget::Transcript;
        self.auto_scroll = false;
        self.transcript_select = Some(TranscriptSelectState {
            selected: self.transcript_items.len().saturating_sub(1),
            pending_args: None,
            pending_g: None,
        });
        self.transcript_select_ensure_visible(self.viewport_height);
        self.status_line = String::from("Transcript select");
    }

    /// Leave transcript-select mode. Safe to call when not in it (no-op apart
    /// from clearing any stale selection state).
    fn exit_transcript_select(&mut self) {
        self.transcript_select = None;
        if self.mode == UiMode::TranscriptSelect {
            self.mode = UiMode::Normal;
            self.focus = FocusTarget::Transcript;
            let max_scroll = self.max_scroll(self.viewport_height);
            self.auto_scroll = self.scroll >= max_scroll;
        }
    }

    fn handle_transcript_select_key(&mut self, key_event: KeyEvent, now: Instant) -> Vec<UiEffect> {
        let Some(mut state) = self.transcript_select else {
            self.exit_transcript_select();
            return Vec::new();
        };
        let last = self.transcript_items.len().saturating_sub(1);
        match key_event.code {
            // Unlike Normal mode, `q` exits the selection, not the app.
            KeyCode::Esc | KeyCode::Char('q') => {
                self.exit_transcript_select();
                return Vec::new();
            }
            KeyCode::Up | KeyCode::Char('k') => {
                state.selected = state.selected.saturating_sub(1);
                state.pending_args = None;
                state.pending_g = None;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                state.selected = state.selected.saturating_add(1).min(last);
                state.pending_args = None;
                state.pending_g = None;
            }
            // `g` is a prefix: `gg` jumps to the top, `gd` opens the selected
            // row's subagent session. `Home` keeps the direct jump.
            KeyCode::Char('g') => {
                state.pending_args = None;
                let chord = state
                    .pending_g
                    .is_some_and(|t| now.duration_since(t) <= GOTO_CHORD_WINDOW);
                if chord {
                    state.pending_g = None;
                    state.selected = 0;
                } else {
                    state.pending_g = Some(now);
                    self.transcript_select = Some(state);
                    return Vec::new();
                }
            }
            KeyCode::Char('d') => {
                let chord = state
                    .pending_g
                    .is_some_and(|t| now.duration_since(t) <= GOTO_CHORD_WINDOW);
                state.pending_g = None;
                if chord {
                    return self.goto_selected_subagent(state);
                }
                self.transcript_select = Some(state);
                return Vec::new();
            }
            KeyCode::Home => {
                state.selected = 0;
                state.pending_args = None;
                state.pending_g = None;
            }
            KeyCode::End | KeyCode::Char('G') => {
                state.selected = last;
                state.pending_args = None;
                state.pending_g = None;
            }
            KeyCode::Char('y') => {
                state.pending_g = None;
                return self.copy_selected_transcript_row(state, now);
            }
            _ => return Vec::new(),
        }
        self.transcript_select = Some(state);
        self.transcript_select_ensure_visible(self.viewport_height);
        Vec::new()
    }

    /// `gd` in transcript-select mode: open the selected `spawn_agent` row's
    /// subagent session as a live preview. The selection and scroll state are
    /// left untouched, so closing the preview restores this view exactly.
    fn goto_selected_subagent(&mut self, state: TranscriptSelectState) -> Vec<UiEffect> {
        self.transcript_select = Some(state);
        let group = self
            .transcript_items
            .get(state.selected)
            .and_then(|item| item.tool_group_cell());
        let Some(group) = group else {
            self.status_line = String::from("Not a subagent row (gd opens spawn_agent sessions)");
            return Vec::new();
        };
        let Some(thread_id) = group.subagent_thread_id() else {
            self.status_line = if group.is_spawn_agent() {
                String::from("Subagent not started yet — no session to open")
            } else {
                String::from("Not a subagent row (gd opens spawn_agent sessions)")
            };
            return Vec::new();
        };
        self.status_line = String::from("Opening subagent…");
        vec![self.effect(
            EffectContext::ThreadPreview { thread_id },
            UiEffectKind::ThreadPreview { thread_id },
        )]
    }

    /// Keys while a subagent preview is open. The top view owns the keyboard
    /// and starts in a Normal-like scroll sub-mode; `Esc Esc` switches it into
    /// a transcript-select sub-mode (mirroring the main view) for `gd`/copy.
    fn handle_preview_key(&mut self, key_event: KeyEvent, now: Instant) -> Vec<UiEffect> {
        let width = self.terminal_width.max(1);
        let viewport = self.viewport_height;
        let select_mode = match self.preview_stack.last() {
            Some(view) => view.select_mode,
            None => return Vec::new(),
        };
        if select_mode {
            self.handle_preview_select_key(key_event, now, width, viewport)
        } else {
            self.handle_preview_scroll_key(key_event, now, width, viewport)
        }
    }

    /// A preview's default sub-mode: Normal-like line/page scrolling with no
    /// selection highlight. `q` pops the preview (one `ThreadUnwatch` per pop —
    /// the server refcounts watchers); `Esc Esc` enters the select sub-mode.
    fn handle_preview_scroll_key(
        &mut self,
        key_event: KeyEvent,
        now: Instant,
        width: u16,
        viewport: u16,
    ) -> Vec<UiEffect> {
        let Some(view) = self.preview_stack.last_mut() else {
            return Vec::new();
        };
        match key_event.code {
            KeyCode::Char('q') => {
                let Some(popped) = self.preview_stack.pop() else {
                    return Vec::new();
                };
                self.status_line = match self.preview_stack.last() {
                    Some(top) => format!("{} — q back", top.header_label()),
                    None => String::from("Closed subagent preview"),
                };
                vec![self.effect(
                    EffectContext::ThreadUnwatch,
                    UiEffectKind::ThreadUnwatch {
                        thread_id: popped.thread_id,
                    },
                )]
            }
            KeyCode::Esc => {
                let chord = view
                    .pending_esc
                    .is_some_and(|t| now.duration_since(t) <= DOUBLE_ESC_WINDOW);
                if chord {
                    view.pending_esc = None;
                    view.pending_g = None;
                    view.select_mode = true;
                    view.selected = view.first_visible_item(width);
                    view.ensure_selected_visible(width, viewport);
                    self.status_line =
                        String::from("Subagent select — j/k move, gd open, Esc back");
                } else {
                    view.pending_esc = Some(now);
                }
                Vec::new()
            }
            KeyCode::Up | KeyCode::Char('k') => {
                view.pending_esc = None;
                view.pending_g = None;
                view.scroll = view.scroll.saturating_sub(1);
                view.auto_scroll = false;
                Vec::new()
            }
            KeyCode::Down | KeyCode::Char('j') => {
                view.pending_esc = None;
                view.pending_g = None;
                let max = view.max_scroll(width, viewport);
                view.scroll = view.scroll.saturating_add(1).min(max);
                view.auto_scroll = view.scroll >= max;
                Vec::new()
            }
            KeyCode::Char('g') => {
                view.pending_esc = None;
                let chord = view
                    .pending_g
                    .is_some_and(|t| now.duration_since(t) <= GOTO_CHORD_WINDOW);
                if chord {
                    view.pending_g = None;
                    view.scroll = 0;
                    view.auto_scroll = false;
                } else {
                    view.pending_g = Some(now);
                }
                Vec::new()
            }
            KeyCode::Char('d') => {
                view.pending_esc = None;
                let chord = view
                    .pending_g
                    .is_some_and(|t| now.duration_since(t) <= GOTO_CHORD_WINDOW);
                view.pending_g = None;
                if chord {
                    self.status_line =
                        String::from("Press Esc Esc to select a row, then gd to open it");
                }
                Vec::new()
            }
            KeyCode::Home => {
                view.pending_esc = None;
                view.pending_g = None;
                view.scroll = 0;
                view.auto_scroll = false;
                Vec::new()
            }
            KeyCode::End | KeyCode::Char('G') => {
                view.pending_esc = None;
                view.pending_g = None;
                view.auto_scroll = true;
                view.scroll_to_bottom(width, viewport);
                Vec::new()
            }
            KeyCode::PageUp => {
                view.pending_esc = None;
                view.pending_g = None;
                view.scroll = view.scroll.saturating_sub(viewport);
                view.auto_scroll = false;
                Vec::new()
            }
            KeyCode::PageDown => {
                view.pending_esc = None;
                view.pending_g = None;
                let max = view.max_scroll(width, viewport);
                view.scroll = view.scroll.saturating_add(viewport).min(max);
                view.auto_scroll = view.scroll >= max;
                Vec::new()
            }
            _ => {
                view.pending_esc = None;
                view.pending_g = None;
                Vec::new()
            }
        }
    }

    /// A preview's transcript-select sub-mode (entered with `Esc Esc`): a
    /// highlighted row, `j`/`k` move the selection, `gg`/`G`/Home/End move it,
    /// `gd` nests a deeper preview, `y`/`yy` copy. `Esc`/`q` return to scroll.
    fn handle_preview_select_key(
        &mut self,
        key_event: KeyEvent,
        now: Instant,
        width: u16,
        viewport: u16,
    ) -> Vec<UiEffect> {
        let Some(view) = self.preview_stack.last_mut() else {
            return Vec::new();
        };
        let last = view.item_count().saturating_sub(1);
        match key_event.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                view.select_mode = false;
                view.pending_g = None;
                view.pending_esc = None;
                view.pending_args = None;
                self.status_line = String::from("Subagent preview — q back, Esc Esc select");
                Vec::new()
            }
            KeyCode::Up | KeyCode::Char('k') => {
                view.pending_g = None;
                view.pending_args = None;
                view.selected = view.selected.saturating_sub(1);
                view.ensure_selected_visible(width, viewport);
                Vec::new()
            }
            KeyCode::Down | KeyCode::Char('j') => {
                view.pending_g = None;
                view.pending_args = None;
                view.selected = view.selected.saturating_add(1).min(last);
                view.ensure_selected_visible(width, viewport);
                Vec::new()
            }
            KeyCode::Char('g') => {
                view.pending_args = None;
                let chord = view
                    .pending_g
                    .is_some_and(|t| now.duration_since(t) <= GOTO_CHORD_WINDOW);
                if chord {
                    view.pending_g = None;
                    view.selected = 0;
                    view.ensure_selected_visible(width, viewport);
                } else {
                    view.pending_g = Some(now);
                }
                Vec::new()
            }
            KeyCode::Char('d') => {
                let chord = view
                    .pending_g
                    .is_some_and(|t| now.duration_since(t) <= GOTO_CHORD_WINDOW);
                view.pending_g = None;
                if !chord {
                    return Vec::new();
                }
                let target = view
                    .selected_tool_group()
                    .map(|group| (group.is_spawn_agent(), group.subagent_thread_id()));
                match target {
                    Some((_, Some(thread_id))) => {
                        self.status_line = String::from("Opening subagent…");
                        vec![self.effect(
                            EffectContext::ThreadPreview { thread_id },
                            UiEffectKind::ThreadPreview { thread_id },
                        )]
                    }
                    Some((true, None)) => {
                        self.status_line =
                            String::from("Subagent not started yet — no session to open");
                        Vec::new()
                    }
                    _ => {
                        self.status_line =
                            String::from("Not a subagent row (gd opens spawn_agent sessions)");
                        Vec::new()
                    }
                }
            }
            KeyCode::Char('y') => self.copy_preview_selected_row(now),
            KeyCode::Home => {
                view.pending_g = None;
                view.pending_args = None;
                view.selected = 0;
                view.ensure_selected_visible(width, viewport);
                Vec::new()
            }
            KeyCode::End | KeyCode::Char('G') => {
                view.pending_g = None;
                view.pending_args = None;
                view.selected = last;
                view.ensure_selected_visible(width, viewport);
                view.auto_scroll = true;
                Vec::new()
            }
            KeyCode::PageUp => {
                view.pending_g = None;
                view.pending_args = None;
                view.scroll = view.scroll.saturating_sub(viewport);
                view.auto_scroll = false;
                Vec::new()
            }
            KeyCode::PageDown => {
                view.pending_g = None;
                view.pending_args = None;
                let max = view.max_scroll(width, viewport);
                view.scroll = view.scroll.saturating_add(viewport).min(max);
                view.auto_scroll = view.scroll >= max;
                Vec::new()
            }
            _ => {
                view.pending_g = None;
                view.pending_args = None;
                Vec::new()
            }
        }
    }

    /// `y` in a preview's select sub-mode. A reduced mirror of
    /// `copy_selected_transcript_row` over the active view's own items: tool
    /// rows copy their result first, a second `y` upgrades to the arguments.
    fn copy_preview_selected_row(&mut self, now: Instant) -> Vec<UiEffect> {
        // (payload, status message, next `pending_args`) resolved from the
        // selected item before the immutable borrow is released.
        type Resolved = (String, &'static str, Option<(usize, Instant)>);
        let Some(view) = self.preview_stack.last_mut() else {
            return Vec::new();
        };
        view.pending_g = None;
        let idx = view.selected;
        let prev_pending = view.pending_args;
        // Resolve the payload from an immutable borrow of the view's items,
        // then release it before mutating `view`/`self`.
        let resolved: Option<Resolved> = {
            let Some(item) = view.selected_item() else {
                view.pending_args = None;
                return Vec::new();
            };
            if let Some(group) = item.tool_group_cell() {
                let chord = prev_pending
                    .is_some_and(|(p, t)| p == idx && now.duration_since(t) <= COPY_CHORD_WINDOW);
                Some(if chord {
                    (group.copy_args(), "Copied tool arguments", None)
                } else if let Some(result) = group.copy_result() {
                    (
                        result,
                        "Copied tool result — y again for arguments",
                        Some((idx, now)),
                    )
                } else {
                    (
                        group.copy_args(),
                        "No result yet — copied tool arguments",
                        None,
                    )
                })
            } else {
                item.copy_text().map(|text| (text, "Copied", None))
            }
        };
        let Some((payload, status, next_pending)) = resolved else {
            view.pending_args = None;
            self.status_line = String::from("Nothing to copy");
            return Vec::new();
        };
        view.pending_args = next_pending;
        let (payload, truncated) = clip_for_clipboard(payload);
        self.status_line = if truncated {
            format!("{status} ({} bytes, truncated)", payload.len())
        } else {
            format!("{status} ({} bytes)", payload.len())
        };
        vec![self.effect(
            EffectContext::Clipboard,
            UiEffectKind::CopyToClipboard { content: payload },
        )]
    }

    /// `y` in transcript-select mode. Tool rows copy their result first; a
    /// second `y` within the chord window upgrades the clipboard to the tool
    /// arguments. Rows with nothing finished yet fall back to the arguments.
    fn copy_selected_transcript_row(
        &mut self,
        mut state: TranscriptSelectState,
        now: Instant,
    ) -> Vec<UiEffect> {
        let idx = state.selected;
        let Some(item) = self.transcript_items.get(idx) else {
            self.transcript_select = Some(state);
            return Vec::new();
        };
        let (payload, status) = if let Some(group) = item.tool_group_cell() {
            let chord = state
                .pending_args
                .is_some_and(|(p, t)| p == idx && now.duration_since(t) <= COPY_CHORD_WINDOW);
            if chord {
                state.pending_args = None;
                (group.copy_args(), "Copied tool arguments")
            } else if let Some(result) = group.copy_result() {
                state.pending_args = Some((idx, now));
                (result, "Copied tool result — y again for arguments")
            } else {
                state.pending_args = None;
                (group.copy_args(), "No result yet — copied tool arguments")
            }
        } else {
            state.pending_args = None;
            match item.copy_text() {
                Some(text) => (text, "Copied"),
                None => {
                    self.transcript_select = Some(state);
                    self.status_line = String::from("Nothing to copy");
                    return Vec::new();
                }
            }
        };
        self.transcript_select = Some(state);
        let (payload, truncated) = clip_for_clipboard(payload);
        self.status_line = if truncated {
            format!("{status} ({} bytes, truncated)", payload.len())
        } else {
            format!("{status} ({} bytes)", payload.len())
        };
        vec![self.effect(
            EffectContext::Clipboard,
            UiEffectKind::CopyToClipboard { content: payload },
        )]
    }

    /// Enter submits only with Ctrl. Cmd/Super is intentionally not accepted:
    /// macOS terminals reserve Cmd for their own bindings (e.g. Ghostty maps
    /// `super+enter` to toggle-fullscreen), so it never reaches the app.
    /// Distinguishing Ctrl+Enter from a bare Enter requires the kitty keyboard
    /// protocol, which `init` enables.
    fn is_submit_key(key_event: KeyEvent) -> bool {
        key_event.code == KeyCode::Enter && key_event.modifiers.contains(KeyModifiers::CONTROL)
    }

    fn handle_insert_key(&mut self, key_event: KeyEvent, now: Instant) -> Vec<UiEffect> {
        // The skill popup is an Insert-mode adornment: it intercepts only
        // navigation/accept/dismiss keys; everything else edits the composer
        // as usual (followed by a popup resync below).
        if self.skill_popup.is_some() {
            match key_event.code {
                _ if Self::is_submit_key(key_event) => {
                    self.skill_popup = None;
                    return self.request_insert_turn_start();
                }
                KeyCode::Esc => {
                    self.skill_popup = None;
                    return Vec::new();
                }
                KeyCode::Up => {
                    if let Some(popup) = self.skill_popup.as_mut() {
                        popup.move_up();
                    }
                    return Vec::new();
                }
                KeyCode::Down => {
                    if let Some(popup) = self.skill_popup.as_mut() {
                        popup.move_down();
                    }
                    return Vec::new();
                }
                KeyCode::Tab | KeyCode::Enter => {
                    self.accept_skill_completion();
                    return Vec::new();
                }
                _ => {}
            }
        }
        let effects = match key_event.code {
            KeyCode::Esc => {
                self.mode = UiMode::Normal;
                self.focus = FocusTarget::Transcript;
                // Arm the chord so Esc-Esc from the composer reaches
                // transcript-select mode.
                self.last_esc = Some(now);
                Vec::new()
            }
            _ if Self::is_submit_key(key_event) => self.request_insert_turn_start(),
            KeyCode::Enter => {
                self.composer.insert_char('\n');
                Vec::new()
            }
            KeyCode::Backspace => {
                self.composer.backspace();
                Vec::new()
            }
            KeyCode::Delete => {
                self.composer.delete();
                Vec::new()
            }
            KeyCode::Tab => {
                self.composer.insert_str("    ");
                Vec::new()
            }
            KeyCode::Left => {
                self.composer.move_left();
                Vec::new()
            }
            KeyCode::Right => {
                self.composer.move_right();
                Vec::new()
            }
            KeyCode::Up => {
                self.composer.move_visual_up(self.composer_inner_width());
                Vec::new()
            }
            KeyCode::Down => {
                self.composer.move_visual_down(self.composer_inner_width());
                Vec::new()
            }
            KeyCode::Home => {
                self.composer.move_line_start();
                Vec::new()
            }
            KeyCode::End => {
                self.composer.move_line_end();
                Vec::new()
            }
            KeyCode::Char(ch)
                if key_event.modifiers.is_empty() || key_event.modifiers == KeyModifiers::SHIFT =>
            {
                self.composer.insert_char(ch);
                Vec::new()
            }
            _ => Vec::new(),
        };
        self.sync_skill_popup();
        effects
    }

    /// Query for the skill popup: `Some(text after the leading slash, up to
    /// the cursor)` while the cursor sits inside a leading `/token` (no
    /// whitespace between the slash and the cursor), `None` otherwise.
    fn skill_popup_query(&self) -> Option<String> {
        let text = self.composer.as_str();
        let rest = text.strip_prefix('/')?;
        let cursor = self.composer.cursor();
        if cursor < 1 {
            return None;
        }
        let before_cursor = rest.get(..cursor - 1)?;
        if before_cursor.contains(char::is_whitespace) {
            return None;
        }
        Some(before_cursor.to_string())
    }

    /// Open, refresh, or dismiss the skill popup to match the composer state.
    /// Skills are rescanned from disk each time the popup transitions to open,
    /// so newly added skills show up without restarting.
    fn sync_skill_popup(&mut self) {
        let Some(query) = self.skill_popup_query() else {
            self.skill_popup = None;
            return;
        };
        if self.skill_popup.is_none() {
            let skills = tools::list_skills(
                &self.skills_root,
                crate::config_state::current().tools.max_skill_bytes,
            );
            if skills.is_empty() {
                return;
            }
            self.skill_popup = Some(SkillPopup::new(skills));
        }
        if let Some(popup) = self.skill_popup.as_mut() {
            popup.set_query(&query);
            if popup.is_empty() {
                self.skill_popup = None;
            }
        }
    }

    /// Replace the leading `/token` in the composer with the selected skill
    /// name and close the popup, leaving the cursor at the end of the text.
    fn accept_skill_completion(&mut self) {
        let Some(name) = self
            .skill_popup
            .as_ref()
            .and_then(|popup| popup.selected_name())
            .map(str::to_string)
        else {
            self.skill_popup = None;
            return;
        };
        let text = self.composer.take_text();
        let token_end = text
            .char_indices()
            .find(|(_, ch)| ch.is_whitespace())
            .map(|(idx, _)| idx)
            .unwrap_or(text.len());
        let remainder = &text[token_end..];
        let replaced = if remainder.is_empty() {
            format!("/{name} ")
        } else {
            format!("/{name}{remainder}")
        };
        self.composer.set_text(replaced);
        self.skill_popup = None;
    }

    fn handle_command_key(&mut self, key_event: KeyEvent) -> Vec<UiEffect> {
        match key_event.code {
            KeyCode::Esc => {
                self.command.clear();
                self.mode = if self.screen == Screen::Workspace
                    && (self.question_picker.is_some() || self.plan_approval.is_some())
                {
                    UiMode::Overlay
                } else {
                    UiMode::Normal
                };
                Vec::new()
            }
            KeyCode::Enter => {
                let command = std::mem::take(&mut self.command);
                self.mode = UiMode::Normal;
                self.execute_command(&command)
            }
            KeyCode::Backspace => {
                self.command.pop();
                Vec::new()
            }
            KeyCode::Char(ch)
                if key_event.modifiers.is_empty() || key_event.modifiers == KeyModifiers::SHIFT =>
            {
                self.command.push(ch);
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn execute_command(&mut self, command: &str) -> Vec<UiEffect> {
        let mut parts = command.split_whitespace();
        match parts.next().unwrap_or_default() {
            "" => Vec::new(),
            "send" => self.request_turn_start(),
            "cancel" => self.request_turn_cancel(),
            "plan" => self.request_plan_toggle(),
            "quit" | "q" => vec![self.effect(EffectContext::Exit, UiEffectKind::Exit)],
            "clear" => {
                self.clear_transcript();
                Vec::new()
            }
            "focus" => {
                let target = parts.next().unwrap_or_default();
                match target {
                    "transcript" | "main" => self.focus = FocusTarget::Transcript,
                    "inspector" | "side" => {
                        self.inspector_visible = true;
                        self.focus = FocusTarget::Inspector;
                    }
                    "composer" | "input" => {
                        self.focus = FocusTarget::Composer;
                        self.mode = UiMode::Insert;
                    }
                    "dashboard" => {
                        self.exit_transcript_select();
                        self.focus = FocusTarget::Dashboard;
                        self.screen = Screen::Dashboard;
                    }
                    _ => self.push_info("usage: :focus transcript|inspector|composer|dashboard"),
                }
                Vec::new()
            }
            "inspector" => {
                let action = parts.next().unwrap_or("toggle");
                match action {
                    "toggle" => self.toggle_inspector_visible(),
                    "show" => self.set_inspector_visible(true),
                    "hide" => self.set_inspector_visible(false),
                    _ => self.push_info("usage: :inspector toggle|show|hide"),
                }
                Vec::new()
            }
            "help" => {
                self.push_info(":send  :cancel  :plan  :quit  :clear  :focus transcript|inspector|composer|dashboard  :inspector toggle|show|hide  I toggle inspector  :help");
                Vec::new()
            }
            other => {
                self.push_error(format!("unknown command: {other}"));
                Vec::new()
            }
        }
    }

    fn dispatch_picker_key(&mut self, key_event: KeyEvent) -> Vec<UiEffect> {
        let outcome = self
            .question_picker
            .as_mut()
            .map(|picker| picker.handle_key(key_event))
            .unwrap_or(PickerOutcome::None);
        match outcome {
            PickerOutcome::None => Vec::new(),
            PickerOutcome::Confirm(response) => {
                let Some(picker) = self.question_picker.take() else {
                    return Vec::new();
                };
                self.mode = UiMode::Normal;
                self.focus = FocusTarget::Transcript;
                let id = self.next_item_id();
                self.push_history(TranscriptItem::question_answers(id, &response.answers));
                vec![self.effect(
                    EffectContext::ServerRequest,
                    UiEffectKind::AnswerQuestion {
                        request_id: picker.request_id,
                        response,
                    },
                )]
            }
            PickerOutcome::Cancel => {
                let Some(picker) = self.question_picker.take() else {
                    return Vec::new();
                };
                self.mode = UiMode::Normal;
                self.focus = FocusTarget::Transcript;
                vec![
                    self.effect(
                        EffectContext::ServerRequest,
                        UiEffectKind::FailQuestion {
                            request_id: picker.request_id,
                            error: JsonRpcError::new(
                                -32001,
                                ErrorInfo::new("user_declined", "user declined to answer")
                                    .with_source("smooth-tui"),
                            ),
                        },
                    ),
                ]
            }
        }
    }

    fn request_insert_turn_start(&mut self) -> Vec<UiEffect> {
        let effects = self.request_turn_start();
        if !effects.is_empty() {
            self.mode = UiMode::Normal;
            self.focus = FocusTarget::Transcript;
        }
        effects
    }

    fn request_turn_start(&mut self) -> Vec<UiEffect> {
        if self.is_turn_running {
            self.push_info("turn already running");
            return Vec::new();
        }

        let input = self.composer.take_text();
        if input.trim().is_empty() {
            return Vec::new();
        }

        let Some(thread_id) = self.current_thread_id else {
            self.push_error("no active thread; start or resume a session before sending");
            self.composer.set_text(input);
            return Vec::new();
        };

        self.status_line = String::from("Starting turn");
        vec![self.effect(
            EffectContext::TurnStart {
                thread_id,
                input: input.clone(),
            },
            UiEffectKind::TurnStart { thread_id, input },
        )]
    }

    fn request_turn_cancel(&mut self) -> Vec<UiEffect> {
        if !self.is_turn_running {
            self.push_info("no running turn to cancel");
            return Vec::new();
        }
        if self.is_turn_cancelling {
            self.status_line = String::from("Cancelling turn");
            return Vec::new();
        }

        let Some(thread_id) = self.current_thread_id else {
            self.push_error("no active thread to cancel");
            return Vec::new();
        };

        self.is_turn_cancelling = true;
        self.status_line = String::from("Cancelling turn");
        vec![self.effect(
            EffectContext::TurnCancel { thread_id },
            UiEffectKind::TurnCancel { thread_id },
        )]
    }

    fn request_plan_toggle(&mut self) -> Vec<UiEffect> {
        let Some(thread_id) = self.current_thread_id else {
            self.push_info("no active thread; start a session before toggling plan mode");
            return Vec::new();
        };
        let previous = self.plan_mode;
        let desired = !self.plan_mode;
        self.plan_mode = desired;
        vec![self.effect(
            EffectContext::SetPlanMode { previous, desired },
            UiEffectKind::SetPlanMode {
                thread_id,
                enabled: desired,
            },
        )]
    }

    fn dispatch_plan_approval_key(&mut self, key_event: KeyEvent) -> Vec<UiEffect> {
        let outcome = self
            .plan_approval
            .as_mut()
            .map(|overlay| overlay.handle_key(key_event))
            .unwrap_or(PlanApprovalOutcome::None);
        match outcome {
            PlanApprovalOutcome::None => Vec::new(),
            PlanApprovalOutcome::Respond(response) => {
                let Some(overlay) = self.plan_approval.take() else {
                    return Vec::new();
                };
                self.mode = UiMode::Normal;
                self.focus = FocusTarget::Transcript;
                vec![self.effect(
                    EffectContext::ServerRequest,
                    UiEffectKind::RespondPlanApproval {
                        request_id: overlay.request_id,
                        response,
                    },
                )]
            }
        }
    }

    fn handle_server_request(&mut self, request: ServerRequest) -> Vec<UiEffect> {
        if let Some(effects) = self.reject_inactive_server_request(&request) {
            return effects;
        }

        match request {
            ServerRequest::AskUserQuestion { request_id, params } => {
                if self.question_picker.is_some() || self.plan_approval.is_some() {
                    return self.fail_server_request(
                        request_id,
                        "another interactive request is already pending".to_string(),
                    );
                }
                // Previewed subagents can ask questions too; close the stack
                // so the overlay is visible and receives keys.
                let effects = self.clear_preview_stack();
                self.screen = Screen::Workspace;
                self.exit_transcript_select();
                self.question_picker = Some(QuestionPicker::new(request_id, params));
                self.mode = UiMode::Overlay;
                self.focus = FocusTarget::Overlay;
                effects
            }
            ServerRequest::RequestPlanApproval { request_id, params } => {
                if self.plan_approval.is_some() || self.question_picker.is_some() {
                    return self.fail_server_request(
                        request_id,
                        "another interactive request is already pending".to_string(),
                    );
                }
                let effects = self.clear_preview_stack();
                self.screen = Screen::Workspace;
                self.exit_transcript_select();
                self.plan_approval = Some(PlanApprovalOverlay::new(request_id, params));
                self.mode = UiMode::Overlay;
                self.focus = FocusTarget::Overlay;
                effects
            }
        }
    }

    fn reject_inactive_server_request(&mut self, request: &ServerRequest) -> Option<Vec<UiEffect>> {
        let (request_id, request_thread_id) = match request {
            ServerRequest::AskUserQuestion { request_id, params } => {
                (request_id.clone(), params.thread_id.as_str())
            }
            ServerRequest::RequestPlanApproval { request_id, params } => {
                (request_id.clone(), params.thread_id.as_str())
            }
        };
        let requested_thread_id = match request_thread_id.parse::<ThreadId>() {
            Ok(thread_id) => thread_id,
            Err(err) => {
                return Some(self.fail_server_request(
                    request_id,
                    format!("invalid server request thread id: {err}"),
                ));
            }
        };
        if self.current_thread_id == Some(requested_thread_id) {
            return None;
        }
        // Previewed subagents are interactive too: their questions and plan
        // approvals are accepted (the preview stack is closed by the handler).
        if self
            .preview_stack
            .iter()
            .any(|view| view.thread_id == requested_thread_id)
        {
            return None;
        }

        Some(self.fail_server_request(
            request_id,
            format!("ignored server request for inactive thread {requested_thread_id}"),
        ))
    }

    fn fail_server_request(&mut self, request_id: RequestId, message: String) -> Vec<UiEffect> {
        vec![self.effect(
            EffectContext::ServerRequest,
            UiEffectKind::FailServerRequest {
                request_id,
                error: JsonRpcError::new(
                    -32000,
                    ErrorInfo::new("server_request_failed", message).with_source("smooth-tui"),
                ),
            },
        )]
    }

    fn apply_effect_result(
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

    /// A successful `threadPreview` response: validate it against the
    /// requested thread and push the view. Any mismatch fails defensively —
    /// the server holds a watcher that only an unwatch will release, and no
    /// view means no pop will ever send one.
    fn apply_thread_preview(
        &mut self,
        context: Option<EffectContext>,
        response: ThreadPreviewResponse,
    ) -> Vec<UiEffect> {
        let requested = match context {
            Some(EffectContext::ThreadPreview { thread_id }) => Some(thread_id),
            _ => None,
        };
        let response_thread = response.thread_id.parse::<ThreadId>().ok();
        let valid = match (requested, response_thread) {
            (Some(requested), Some(actual)) => requested == actual,
            _ => false,
        };
        if !valid {
            self.status_line = String::from("Could not open subagent: unexpected response");
            // Unwatch whichever id the server may have taken a watcher for.
            return requested
                .or(response_thread)
                .map(|thread_id| {
                    vec![self.effect(
                        EffectContext::ThreadUnwatch,
                        UiEffectKind::ThreadUnwatch { thread_id },
                    )]
                })
                .unwrap_or_default();
        }

        let width = self.terminal_width.max(1);
        let mut view = SubagentPreviewView::from_preview_response(response, width);
        view.scroll_to_bottom(width, self.viewport_height);
        self.status_line = format!("{} — q back", view.header_label());
        self.preview_stack.push(view);
        Vec::new()
    }

    /// Pop every preview view, emitting one `ThreadUnwatch` per view — the
    /// server refcounts watchers, so each successful preview needs exactly
    /// one release, duplicates included.
    fn clear_preview_stack(&mut self) -> Vec<UiEffect> {
        let views = std::mem::take(&mut self.preview_stack);
        views
            .into_iter()
            .map(|view| {
                self.effect(
                    EffectContext::ThreadUnwatch,
                    UiEffectKind::ThreadUnwatch {
                        thread_id: view.thread_id,
                    },
                )
            })
            .collect()
    }

    fn apply_effect_failure(&mut self, effect_id: EffectId, error: String) -> Vec<UiEffect> {
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

    fn apply_thread_start_response(&mut self, response: ThreadStartResponse) {
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

    fn apply_thread_resume_response(
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

    fn apply_protocol_event(&mut self, event: Event) {
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
                // `gd` can resolve the row to a subagent session later.
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

    fn handle_assistant_delta(&mut self, delta: String) {
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

    fn handle_reasoning_delta(&mut self, item_id: String, delta: String) {
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

    /// Set the active assistant lines and invalidate the active-wrap cache by
    /// bumping the version. All writes to the active lines go through here so the
    /// memo in `refresh_active_wrap` can never go stale.
    fn set_active_assistant_lines(&mut self, lines: Option<Vec<Line<'static>>>) {
        self.active_assistant_lines = lines;
        self.active_version = self.active_version.wrapping_add(1);
    }

    fn set_active_reasoning_lines(&mut self, lines: Option<Vec<Line<'static>>>) {
        self.active_reasoning_lines = lines;
        self.active_version = self.active_version.wrapping_add(1);
    }

    /// Ensure `active_wrap` holds the active streams wrapped at `width` for the
    /// current `active_version`, recomputing only on a miss. The active streams
    /// mutate every delta so they stay out of `render_cache`; this memo keeps
    /// them from being re-wrapped twice per frame (row count + visible lines)
    /// and on idle frames where nothing streamed.
    fn refresh_active_wrap(&mut self, width: u16) {
        if self
            .active_wrap
            .as_ref()
            .is_some_and(|cache| cache.width == width && cache.version == self.active_version)
        {
            return;
        }
        #[cfg(test)]
        {
            self.active_wrap_computes += 1;
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

    fn finalize_assistant_stream(&mut self, item_id: Option<&str>) -> bool {
        self.finalize_reasoning_stream();
        self.commit_assistant_stream(item_id)
    }

    fn commit_assistant_stream(&mut self, item_id: Option<&str>) -> bool {
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

    fn finalize_reasoning_stream(&mut self) -> bool {
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

    fn push_rendered_assistant_message(&mut self, message: &str) {
        let lines = self.render_markdown_lines(message);
        let id = self.next_item_id();
        self.push_history(TranscriptItem::assistant(
            id,
            lines,
            true,
            message.to_owned(),
        ));
    }

    fn push_rendered_reasoning_message(&mut self, message: &str) {
        let lines = self.render_markdown_lines(message);
        let id = self.next_item_id();
        self.push_history(TranscriptItem::reasoning(
            id,
            lines,
            true,
            message.to_owned(),
        ));
    }

    fn render_markdown_lines(&self, message: &str) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        crate::markdown::append_markdown(
            message,
            Some(usize::from(self.terminal_width.saturating_sub(6))),
            &mut lines,
        );
        lines
    }

    fn push_history(&mut self, item: TranscriptItem) {
        self.pending_tool_group = None;
        self.transcript_items.push(item);
    }

    fn push_info(&mut self, message: impl Into<String>) {
        let id = self.next_item_id();
        self.push_history(TranscriptItem::info(id, message));
    }

    fn push_error(&mut self, message: impl Into<String>) {
        let id = self.next_item_id();
        self.push_history(TranscriptItem::error(id, message));
    }

    fn push_tool_group_cell(&mut self, cell: ToolCallGroupCell) -> usize {
        let cell_idx = self.transcript_items.len();
        let tool_name = cell.tool_name().to_owned();
        let id = self.next_item_id();
        self.transcript_items
            .push(TranscriptItem::tool_group(id, cell));
        self.pending_tool_group = Some((cell_idx, tool_name));
        cell_idx
    }

    fn tool_group_mut(&mut self, cell_idx: usize) -> &mut ToolCallGroupCell {
        match self
            .transcript_items
            .get_mut(cell_idx)
            .and_then(TranscriptItem::tool_group_mut)
        {
            Some(group) => group,
            None => panic!("tracked tool row should be a ToolCallGroupCell"),
        }
    }

    fn mark_item_mutated(&mut self, cell_idx: usize) {
        if let Some(item) = self.transcript_items.get_mut(cell_idx) {
            item.mark_mutated();
        }
    }

    fn replace_tool_call_with_file_changes(
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
    fn replace_tool_call_with_todo_list(&mut self, call_id: &str, todos: Vec<TodoItem>) -> bool {
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

    fn complete_subagent_tool_call(&mut self, child_thread_id: ThreadId, status: &AgentStatus) {
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

    fn clear_transcript(&mut self) {
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

    fn reset_turn_tracking(&mut self) {
        self.is_turn_running = false;
        self.is_turn_cancelling = false;
        self.current_turn_id = None;
        self.committed_assistant_item_id = None;
        self.committed_assistant_for_current_turn = false;
        self.committed_reasoning_item_id = None;
        self.in_flight_reasoning_item_id = None;
        self.plan_mode = false;
    }

    #[cfg(test)]
    fn transcript_lines_uncached(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        let item_count = self.transcript_items.len();
        let has_active_below = self.has_active_stream_lines();
        for (idx, item) in self.transcript_items.iter().enumerate() {
            if idx > 0 {
                lines.push(Line::default());
            }
            let mut item_lines = item.display_lines(width);
            if idx + 1 == item_count && !has_active_below && item.is_user() {
                item_lines.pop();
            }
            lines.extend(item_lines);
        }
        self.append_active_lines(&mut lines, width);
        if lines.is_empty() {
            lines.push(
                Line::from("No transcript yet. Type a message and use :send.")
                    .style(Style::default().dim()),
            );
        }
        lines
    }

    /// Whether an active reasoning/assistant stream is currently rendered below
    /// the committed transcript items. Used to decide whether the last user
    /// message has anything following it: when nothing does, its bottom
    /// separator is dropped so the transcript doesn't end on a dangling rule.
    fn has_active_stream_lines(&self) -> bool {
        self.active_reasoning_lines
            .as_ref()
            .is_some_and(|lines| !lines.is_empty())
            || self
                .active_assistant_lines
                .as_ref()
                .is_some_and(|lines| !lines.is_empty())
    }

    fn visible_transcript_lines(&mut self, width: u16, viewport_height: u16) -> Vec<Line<'static>> {
        let start = usize::from(self.scroll);
        let end = usize::from(self.scroll).saturating_add(usize::from(viewport_height));
        let mut all_rows = 0usize;
        let mut lines = Vec::new();

        let item_count = self.transcript_items.len();
        let has_active_below = self.has_active_stream_lines();
        let selected_idx = if self.mode == UiMode::TranscriptSelect {
            self.transcript_select.map(|state| state.selected)
        } else {
            None
        };
        self.render_cache.evict_stale_widths(width);
        for (idx, item) in self.transcript_items.iter().enumerate() {
            if idx > 0 {
                append_visible_line(&mut lines, Line::default(), all_rows, start, end);
                all_rows += 1;
            }
            let omit_bottom = idx + 1 == item_count && !has_active_below && item.is_user();
            let mut item_height = self.render_cache.item_height(item, width);
            if omit_bottom {
                item_height = item_height.saturating_sub(1);
            }
            if all_rows >= end || all_rows.saturating_add(item_height) <= start {
                all_rows = all_rows.saturating_add(item_height);
                continue;
            }

            let mut item_lines = self.render_cache.item_lines(item, width);
            if omit_bottom {
                item_lines.pop();
            }
            // Highlight the selected row by patching the per-frame clone of
            // the cached lines; the cache itself stays untouched.
            if selected_idx == Some(idx) {
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
        let Some(active) = self.active_wrap.as_ref() else {
            return lines;
        };
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

        if all_rows == 0 {
            lines.push(
                Line::from("No transcript yet. Type a message and use :send.")
                    .style(Style::default().dim()),
            );
        }

        lines
    }

    #[cfg(test)]
    fn append_active_lines(&self, lines: &mut Vec<Line<'static>>, width: u16) {
        if let Some(active_lines) = self.active_reasoning_lines.as_ref() {
            if !lines.is_empty() {
                lines.push(Line::default());
            }
            let item = TranscriptItem::reasoning(0, active_lines.clone(), true, String::new());
            lines.extend(item.display_lines(width));
        }
        if let Some(active_lines) = self.active_assistant_lines.as_ref() {
            if !lines.is_empty() {
                lines.push(Line::default());
            }
            let item = TranscriptItem::assistant(0, active_lines.clone(), true, String::new());
            lines.extend(item.display_lines(width));
        }
    }

    fn total_transcript_rows(&mut self, width: u16) -> usize {
        if self.transcript_items.is_empty()
            && self.active_assistant_lines.is_none()
            && self.active_reasoning_lines.is_none()
        {
            return 1;
        }

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
            rows += height;
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

    fn render(&mut self, frame: &mut Frame<'_>) {
        self.terminal_width = frame.area().width;
        match self.screen {
            Screen::Dashboard => self.render_dashboard(frame, frame.area()),
            Screen::Workspace if !self.preview_stack.is_empty() => {
                self.render_preview(frame, frame.area());
            }
            Screen::Workspace => self.render_workspace(frame, frame.area()),
        }
    }

    /// Full-screen subagent preview: a header naming the agent and its live
    /// status, the read-only transcript, and a key-hint footer.
    fn render_preview(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .split(area);
        let depth = self.preview_stack.len();
        let Some(view) = self.preview_stack.last_mut() else {
            return;
        };
        let width = chunks[1].width.max(1);
        let viewport = chunks[1].height.max(1);
        let select_mode = view.select_mode;

        let mut header = view.header_label();
        if depth > 1 {
            header = format!("{header} · nested ×{}", depth - 1);
        }
        frame.render_widget(
            Paragraph::new(separator_line(
                area.width,
                &header,
                Style::default().fg(Color::Cyan),
            )),
            chunks[0],
        );

        if view.auto_scroll {
            view.scroll_to_bottom(width, viewport);
        }
        let lines = view.visible_lines(width, viewport);
        frame.render_widget(Paragraph::new(Text::from(lines)), chunks[1]);

        // The status line stays visible inside the preview: in-preview
        // feedback ("Subagent not started yet", failed nested opens) would
        // otherwise be invisible until the user exits.
        let hint = if select_mode {
            "j/k move  gg/G top/bottom  gd nested  y copy  Esc back"
        } else {
            "j/k scroll  gg/G top/bottom  Esc Esc select  q back"
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::raw(self.status_line.clone()),
                Span::raw("  "),
                Span::styled(hint, Style::default().fg(Color::DarkGray)),
            ])),
            chunks[2],
        );
    }

    fn render_dashboard(&self, frame: &mut Frame<'_>, area: Rect) {
        self.render_dashboard_body(frame, area);
    }

    fn render_dashboard_body(&self, frame: &mut Frame<'_>, area: Rect) {
        let mut lines = Vec::new();
        lines.push(Line::from(vec![
            Span::styled("smooth-code", Style::default().bold().fg(Color::Cyan)),
            Span::raw("  "),
            Span::styled("sessions", Style::default().dim()),
        ]));
        lines.push(Line::default());
        if self.dashboard.loading {
            lines.push(Line::from(Span::styled(
                "Loading recent sessions...",
                Style::default().dim(),
            )));
        } else if let Some(error) = &self.dashboard.error {
            lines.extend(wrap::wrap_line_hanging(
                Line::from(vec![
                    Span::styled("! ", Style::default().fg(Color::Red).bold()),
                    Span::styled(error.clone(), Style::default().fg(Color::Red)),
                ]),
                usize::from(area.width.max(1)),
                2,
            ));
        } else if self.dashboard.items.is_empty() {
            lines.push(Line::from("No saved sessions."));
        } else {
            let visible_count = self.dashboard_visible_item_count(area.height);
            let max_offset = self.dashboard_max_scroll_offset(visible_count);
            let start = self.dashboard.scroll_offset.min(max_offset);
            let end = start
                .saturating_add(visible_count)
                .min(self.dashboard.items.len());
            for (idx, item) in self.dashboard.items[start..end].iter().enumerate() {
                let item_idx = start + idx;
                let selected = item_idx == self.dashboard.selected;
                let prefix = if selected { "› " } else { "  " };
                let style = if selected {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                let preview = item
                    .last_user_message
                    .as_deref()
                    .or(item.last_assistant_message.as_deref())
                    .unwrap_or("(no messages)");
                // Keep each session two rows tall: truncate the preview to the
                // space left after the selection prefix, date, and gap so it
                // never wraps and breaks the dashboard scroll math.
                let used = wrap::display_width(prefix) + wrap::display_width(&item.updated_at) + 2;
                let preview = wrap::truncate_display(
                    preview,
                    usize::from(area.width.max(1)).saturating_sub(used),
                );
                lines.push(Line::from(vec![
                    Span::styled(prefix, style),
                    Span::styled(item.updated_at.clone(), Style::default().fg(Color::Yellow)),
                    Span::raw("  "),
                    Span::styled(preview, style),
                ]));
                lines.push(Line::from(vec![
                    Span::raw("    "),
                    Span::styled(item.thread_id.clone(), Style::default().fg(Color::DarkGray)),
                ]));
            }
        }
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "n new  Enter resume  j/k move  PgUp/PgDn scroll  : command  Ctrl-C quit",
            Style::default().fg(Color::DarkGray),
        )));

        frame.render_widget(Paragraph::new(lines), area);
    }

    fn render_workspace(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let picker_height = self
            .question_picker
            .as_ref()
            .map(|picker| picker.desired_height(area.width).min(20))
            .unwrap_or(0);
        let approval_height = self
            .plan_approval
            .as_ref()
            .map(|overlay| overlay.desired_height(area.width).min(30))
            .unwrap_or(0);
        let command_height = if self.mode == UiMode::Command { 1 } else { 0 };
        let composer_height = self.composer_height();
        let skill_popup_height = self
            .skill_popup
            .as_ref()
            .map(|popup| popup.desired_height().min(8))
            .unwrap_or(0);

        let mut constraints = Vec::new();
        constraints.push(Constraint::Min(5));
        if picker_height > 0 {
            constraints.push(Constraint::Length(picker_height));
        }
        if approval_height > 0 {
            constraints.push(Constraint::Length(approval_height));
        }
        if skill_popup_height > 0 {
            constraints.push(Constraint::Length(skill_popup_height));
        }
        constraints.push(Constraint::Length(1));
        constraints.push(Constraint::Length(composer_height));
        if command_height > 0 {
            constraints.push(Constraint::Length(command_height));
        }
        constraints.push(Constraint::Length(1));
        constraints.push(Constraint::Length(1));

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(area);

        let mut idx = 0;
        self.render_workspace_body(frame, chunks[idx]);
        idx += 1;
        if picker_height > 0 {
            if let Some(picker) = &self.question_picker {
                picker.render(frame, chunks[idx]);
            }
            idx += 1;
        }
        if approval_height > 0 {
            if let Some(overlay) = &self.plan_approval {
                overlay.render(frame, chunks[idx]);
            }
            idx += 1;
        }
        if skill_popup_height > 0 {
            if let Some(popup) = &self.skill_popup {
                popup.render(frame, chunks[idx]);
            }
            idx += 1;
        }
        render_horizontal_separator(
            frame,
            chunks[idx],
            self.composer_title(),
            self.composer_accent_style(),
        );
        idx += 1;
        self.render_composer(frame, chunks[idx]);
        idx += 1;
        if command_height > 0 {
            self.render_command(frame, chunks[idx]);
            idx += 1;
        }
        render_horizontal_separator(frame, chunks[idx], "Status", muted_separator_style());
        idx += 1;
        self.render_status(frame, chunks[idx]);
    }

    fn render_workspace_body(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let show_split = area.width >= 110 && self.inspector_visible;
        if show_split {
            let chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Percentage(70),
                    Constraint::Length(1),
                    Constraint::Percentage(30),
                ])
                .split(area);
            self.render_transcript(frame, chunks[0]);
            render_vertical_separator(frame, chunks[1]);
            self.render_inspector(frame, chunks[2]);
        } else if self.inspector_visible && self.focus == FocusTarget::Inspector {
            self.render_inspector(frame, area);
        } else {
            self.render_transcript(frame, area);
        }
    }

    fn render_transcript(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let inner_width = area.width.max(1);
        let viewport_height = area.height.max(1);
        // Record the width we actually wrap/draw at so `max_scroll` (which also
        // runs from key handlers, before the next draw) counts the same rows.
        self.transcript_inner_width = inner_width;
        if self.auto_scroll {
            self.scroll_to_bottom(viewport_height);
        }
        let lines = self.visible_transcript_lines(inner_width, viewport_height);
        let paragraph = Paragraph::new(Text::from(lines));
        frame.render_widget(paragraph, area);
    }

    fn render_inspector(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let wrap_width = usize::from(area.width.max(1));
        let mut lines = Vec::new();
        lines.push(Line::from(Span::styled(
            "Inspector",
            Style::default().fg(Color::Cyan).bold(),
        )));
        lines.push(Line::default());
        lines.extend(wrap::wrap_line_hanging(
            Line::from(vec![
                Span::styled("Turn ", Style::default().fg(Color::Yellow).bold()),
                Span::raw(
                    self.current_turn_id
                        .as_deref()
                        .unwrap_or(if self.is_turn_running {
                            "running"
                        } else {
                            "idle"
                        })
                        .to_string(),
                ),
            ]),
            wrap_width,
            wrap::display_width("Turn "),
        ));
        lines.extend(wrap::wrap_line_hanging(
            Line::from(vec![
                Span::styled("Mode ", Style::default().fg(Color::Yellow).bold()),
                Span::raw(format!("{:?}", self.mode)),
                Span::raw("  "),
                Span::styled(
                    if self.plan_mode { "PLAN" } else { "FULL" },
                    if self.plan_mode {
                        Style::default().fg(Color::Magenta).bold()
                    } else {
                        Style::default().dim()
                    },
                ),
            ]),
            wrap_width,
            wrap::display_width("Mode "),
        ));
        if let Some(thread_id) = self.current_thread_id {
            lines.extend(wrap::wrap_line_hanging(
                Line::from(vec![
                    Span::styled("Thread ", Style::default().fg(Color::Yellow).bold()),
                    Span::styled(thread_id.to_string(), Style::default().fg(Color::DarkGray)),
                ]),
                wrap_width,
                wrap::display_width("Thread "),
            ));
        }

        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "Running Tools",
            Style::default().bold(),
        )));
        if self.running_tools.is_empty() {
            lines.push(Line::from(Span::styled("none", Style::default().dim())));
        } else {
            for tool in self.running_tools.values() {
                // Align continuation rows under the args text (glyph + name + space).
                let indent = 2 + wrap::display_width(&tool.tool_name) + 1;
                lines.extend(wrap::wrap_line_hanging(
                    Line::from(vec![
                        Span::styled("⠋ ", Style::default().fg(Color::Yellow).bold()),
                        Span::raw(tool.tool_name.clone()),
                        Span::raw(" "),
                        Span::styled(tool.args_preview.clone(), Style::default().dim()),
                    ]),
                    wrap_width,
                    indent,
                ));
            }
        }

        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            "Recent File Changes",
            Style::default().bold(),
        )));
        if self.recent_file_changes.is_empty() {
            lines.push(Line::from(Span::styled("none", Style::default().dim())));
        } else {
            for change in self.recent_file_changes.iter().rev().take(4) {
                lines.extend(wrap::wrap_line_hanging(
                    Line::from(vec![
                        Span::raw("• "),
                        Span::raw(file_change_path_label(change)),
                    ]),
                    wrap_width,
                    2,
                ));
            }
        }

        frame.render_widget(Paragraph::new(lines), area);
    }

    fn render_status(&self, frame: &mut Frame<'_>, area: Rect) {
        let mut spans = Vec::with_capacity(10);
        spans.push(Span::styled(
            "Status ",
            Style::default().fg(Color::Yellow).bold(),
        ));
        spans.push(Span::raw(self.status_line.clone()));
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            if self.is_turn_cancelling {
                "agent cancelling"
            } else if self.is_turn_running {
                "agent running"
            } else {
                "agent idle"
            },
            Style::default().dim(),
        ));
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!("{:?}", self.mode),
            Style::default().fg(Color::Cyan),
        ));
        if self.mode == UiMode::TranscriptSelect {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                "j/k move  y copy  yy copy args  gg top  G bottom  gd subagent  Esc exit",
                Style::default().fg(Color::DarkGray),
            ));
        }
        if self.plan_mode {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                "⏸ PLAN MODE",
                Style::default().fg(Color::Magenta).bold(),
            ));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn render_command(&self, frame: &mut Frame<'_>, area: Rect) {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(":", Style::default().fg(Color::Cyan).bold()),
                Span::raw(self.command.clone()),
            ])),
            area,
        );
    }

    fn render_composer(&self, frame: &mut Frame<'_>, area: Rect) {
        let composer_width = area.width.max(1);
        let visible_height = area.height.max(1);
        let rows = self.composer.visual_rows(usize::from(composer_width));
        let (cursor_row, cursor_col) = self.composer.cursor_visual_position_in_rows(&rows);
        let scroll_row = cursor_row
            .saturating_add(1)
            .saturating_sub(usize::from(visible_height));
        let visible_lines: Vec<Line<'static>> = rows
            .iter()
            .skip(scroll_row)
            .take(usize::from(visible_height))
            .map(|row| Line::raw(self.composer.as_str()[row.start..row.end].to_owned()))
            .collect();

        frame.render_widget(Paragraph::new(Text::from(visible_lines)), area);

        if self.mode == UiMode::Insert && !self.is_turn_running {
            let inner_width = usize::from(composer_width);
            let x_offset = cursor_col.min(inner_width.saturating_sub(1));
            let y_offset = cursor_row
                .saturating_sub(scroll_row)
                .min(usize::from(visible_height.saturating_sub(1)));
            let x = area
                .x
                .saturating_add(u16::try_from(x_offset).unwrap_or(u16::MAX));
            let y = area
                .y
                .saturating_add(u16::try_from(y_offset).unwrap_or(u16::MAX));
            frame.set_cursor_position((x, y.min(area.y.saturating_add(area.height - 1))));
        }
    }

    fn composer_title(&self) -> &'static str {
        if self.plan_mode {
            "Input (plan)"
        } else {
            "Input"
        }
    }

    fn composer_accent_style(&self) -> Style {
        if self.plan_mode {
            Style::default().fg(Color::Magenta)
        } else if self.is_turn_running {
            Style::default().fg(Color::DarkGray)
        } else if self.mode == UiMode::Insert {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        }
    }

    fn composer_height(&self) -> u16 {
        let rows = self
            .composer
            .visual_rows(self.composer_inner_width())
            .len()
            .max(1);
        u16::try_from(rows).unwrap_or(4).clamp(1, 4)
    }

    fn composer_inner_width(&self) -> usize {
        usize::from(self.terminal_width.max(1))
    }

    fn transcript_cache_width_hint(&self, terminal_width: u16) -> u16 {
        if self.screen == Screen::Workspace && terminal_width >= 110 && self.inspector_visible {
            let available = u32::from(terminal_width.saturating_sub(1));
            u16::try_from(available.saturating_mul(70) / 100).unwrap_or(u16::MAX)
        } else {
            terminal_width.max(1)
        }
    }

    fn transcript_viewport_height(&self, width: u16, height: u16) -> u16 {
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
        let approval_height = self
            .plan_approval
            .as_ref()
            .map(|overlay| overlay.desired_height(width).min(30))
            .unwrap_or(0);
        let command_height = if self.mode == UiMode::Command { 1 } else { 0 };
        height
            .saturating_sub(picker_height)
            .saturating_sub(approval_height)
            .saturating_sub(1)
            .saturating_sub(1)
            .saturating_sub(command_height)
            .saturating_sub(1)
            .saturating_sub(self.composer_height())
            .max(1)
    }

    fn focus_next(&mut self) {
        self.focus = match self.focus {
            FocusTarget::Dashboard => FocusTarget::Transcript,
            FocusTarget::Transcript if self.inspector_visible => FocusTarget::Inspector,
            FocusTarget::Transcript => FocusTarget::Composer,
            FocusTarget::Inspector => FocusTarget::Composer,
            FocusTarget::Composer => FocusTarget::Transcript,
            FocusTarget::Overlay => FocusTarget::Transcript,
        };
    }

    fn focus_prev(&mut self) {
        self.focus = match self.focus {
            FocusTarget::Dashboard => FocusTarget::Composer,
            FocusTarget::Transcript => FocusTarget::Composer,
            FocusTarget::Inspector => FocusTarget::Transcript,
            FocusTarget::Composer if self.inspector_visible => FocusTarget::Inspector,
            FocusTarget::Composer => FocusTarget::Transcript,
            FocusTarget::Overlay => FocusTarget::Transcript,
        };
    }

    fn set_inspector_visible(&mut self, visible: bool) {
        self.inspector_visible = visible;
        if !visible && self.focus == FocusTarget::Inspector {
            self.focus = FocusTarget::Transcript;
        }
    }

    fn toggle_inspector_visible(&mut self) {
        self.set_inspector_visible(!self.inspector_visible);
    }

    fn dashboard_visible_item_count(&self, height: u16) -> usize {
        usize::from(height.saturating_sub(4) / 2).max(1)
    }

    fn dashboard_max_scroll_offset(&self, visible_count: usize) -> usize {
        self.dashboard
            .items
            .len()
            .saturating_sub(visible_count.max(1))
    }

    fn dashboard_ensure_selected_visible(&mut self, height: u16) {
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

    fn scroll_up(&mut self, amount: u16) {
        self.scroll = self.scroll.saturating_sub(amount);
        self.auto_scroll = false;
    }

    /// Row range `(start, height)` of the item at `target_idx`, walking items
    /// exactly like `total_transcript_rows`: one separator row before each
    /// idx>0 item, cached heights, and the trailing user rule omitted when
    /// nothing streams below it.
    fn transcript_item_row_extent(&mut self, target_idx: usize, width: u16) -> (usize, usize) {
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

    /// Scroll just enough that the selected transcript row is on screen;
    /// items taller than the viewport pin to their top.
    fn transcript_select_ensure_visible(&mut self, viewport_height: u16) {
        let Some(state) = self.transcript_select else {
            return;
        };
        let (start, height) =
            self.transcript_item_row_extent(state.selected, self.transcript_inner_width);
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

    fn scroll_down(&mut self, amount: u16, viewport_height: u16) {
        let max_scroll = self.max_scroll(viewport_height);
        self.scroll = self.scroll.saturating_add(amount).min(max_scroll);
        self.auto_scroll = self.scroll >= max_scroll;
    }

    fn scroll_to_bottom(&mut self, viewport_height: u16) {
        self.scroll = self.max_scroll(viewport_height);
    }

    fn max_scroll(&mut self, viewport_height: u16) -> u16 {
        let inner_width = self.transcript_inner_width;
        let total_rows = self.total_transcript_rows(inner_width);
        let max_scroll = total_rows.saturating_sub(usize::from(viewport_height));
        u16::try_from(max_scroll).unwrap_or(u16::MAX)
    }
}

fn muted_separator_style() -> Style {
    Style::default().fg(Color::DarkGray)
}

/// Copy `content` to the system clipboard via the OSC 52 escape sequence.
/// Written straight to stdout: an OSC sequence does not disturb the ratatui
/// cell grid, and support is up to the terminal (unsupported ones ignore it).
fn write_clipboard_osc52(content: &str) -> TuiResult<()> {
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(content.as_bytes());
    crossterm::execute!(
        std::io::stdout(),
        crossterm::style::Print(format!("\x1b]52;c;{encoded}\x07"))
    )?;
    Ok(())
}

/// Cap clipboard payloads at `MAX_CLIPBOARD_BYTES` on a char boundary; returns
/// whether anything was dropped.
fn clip_for_clipboard(mut content: String) -> (String, bool) {
    if content.len() <= MAX_CLIPBOARD_BYTES {
        return (content, false);
    }
    let mut cut = MAX_CLIPBOARD_BYTES;
    while !content.is_char_boundary(cut) {
        cut -= 1;
    }
    content.truncate(cut);
    (content, true)
}

fn render_horizontal_separator(frame: &mut Frame<'_>, area: Rect, label: &str, style: Style) {
    if area.height == 0 {
        return;
    }
    frame.render_widget(
        Paragraph::new(separator_line(area.width, label, style)),
        area,
    );
}

fn render_vertical_separator(frame: &mut Frame<'_>, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let lines = (0..area.height)
        .map(|_| Line::from(Span::styled("│", muted_separator_style())))
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(lines), area);
}

pub(crate) fn separator_line(width: u16, label: &str, style: Style) -> Line<'static> {
    let width = usize::from(width);
    if width == 0 {
        return Line::default();
    }

    let text = if label.is_empty() {
        "─".repeat(width)
    } else {
        let prefix = format!("─ {label} ");
        let prefix_len = prefix.chars().count();
        if prefix_len >= width {
            prefix.chars().take(width).collect()
        } else {
            format!("{prefix}{}", "─".repeat(width - prefix_len))
        }
    };
    Line::from(Span::styled(text, style))
}

pub(crate) fn append_visible_line(
    lines: &mut Vec<Line<'static>>,
    line: Line<'static>,
    row: usize,
    start: usize,
    end: usize,
) {
    if row >= start && row < end {
        lines.push(line);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RenderCacheKey {
    item_id: TranscriptItemId,
    version: u64,
    width: u16,
}

/// Wrapped active assistant/reasoning streams cached for one `(width, version)`.
#[derive(Debug, Clone)]
pub(crate) struct ActiveWrap {
    pub(crate) width: u16,
    pub(crate) version: u64,
    pub(crate) reasoning: Vec<Line<'static>>,
    pub(crate) assistant: Vec<Line<'static>>,
}

#[derive(Debug, Default)]
pub(crate) struct RenderedTranscriptCache {
    entries: HashMap<RenderCacheKey, CachedRenderedRows>,
    width: Option<u16>,
}

#[derive(Debug, Clone)]
struct CachedRenderedRows {
    lines: Vec<Line<'static>>,
}

impl RenderedTranscriptCache {
    pub(crate) fn item_lines(&mut self, item: &TranscriptItem, width: u16) -> Vec<Line<'static>> {
        self.item_entry(item, width).lines.clone()
    }

    pub(crate) fn item_height(&mut self, item: &TranscriptItem, width: u16) -> usize {
        self.item_entry(item, width).lines.len()
    }

    fn item_entry(&mut self, item: &TranscriptItem, width: u16) -> &CachedRenderedRows {
        let key = RenderCacheKey {
            item_id: item.id(),
            version: item.version(),
            width,
        };
        self.entries
            .entry(key)
            .or_insert_with(|| CachedRenderedRows {
                lines: item.display_lines(width),
            })
    }

    pub(crate) fn evict_stale_widths(&mut self, width: u16) {
        if self.width == Some(width) {
            return;
        }
        self.width = Some(width);
        self.entries.retain(|key, _| key.width == width);
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.width = None;
    }
}

pub(crate) fn agent_status_label(status: &AgentStatus) -> String {
    match status {
        AgentStatus::PendingInit => String::from("pending"),
        AgentStatus::Running => String::from("running"),
        AgentStatus::Interrupted => String::from("interrupted"),
        AgentStatus::Completed(Some(text)) => format!("completed ({text})"),
        AgentStatus::Completed(None) => String::from("completed"),
        AgentStatus::Errored(error) => format!("errored ({})", error.message),
        AgentStatus::Shutdown => String::from("shutdown"),
        AgentStatus::NotFound => String::from("not_found"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::streaming::StreamController;
    use app_server_protocol::{
        AskUserQuestion, AskUserQuestionOption, AskUserQuestionParams, PlanApprovalDecision,
        RequestPlanApprovalParams,
    };
    use ratatui::{Terminal, backend::TestBackend};
    use smooth_protocol::{
        AgentMessageCompletedEvent, AgentMessageDeltaEvent, AgentReasoningCompletedEvent,
        AgentReasoningDeltaEvent, CollabAgentSpawnBeginEvent, CollabAgentSpawnEndEvent,
        CollabResumeBeginEvent, CollabResumeEndEvent, EventMsg, StreamErrorEvent,
        ToolCallCompletedEvent, ToolCallStartedEvent, TurnCompletedEvent, TurnInterruptedEvent,
        TurnStartedEvent,
    };

    fn event(id: &str, msg: EventMsg) -> Event {
        Event {
            id: id.to_owned(),
            msg,
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn modified_key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn workspace_insert_model() -> UiModel {
        let mut model = UiModel::new();
        model.screen = Screen::Workspace;
        model.mode = UiMode::Insert;
        model.focus = FocusTarget::Composer;
        model
    }

    fn workspace_normal_model() -> UiModel {
        let mut model = UiModel::new();
        model.screen = Screen::Workspace;
        model.mode = UiMode::Normal;
        model.focus = FocusTarget::Transcript;
        model
    }

    fn transcript_strings(app: &App) -> Vec<String> {
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

    fn rendered_buffer_text(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    fn buffer_rows(terminal: &Terminal<TestBackend>, width: usize) -> Vec<String> {
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

    fn dashboard_thread(idx: usize) -> ThreadListItem {
        ThreadListItem {
            thread_id: format!("thread-{idx}"),
            rollout_path: format!("session-{idx}.jsonl"),
            created_at: "2026-05-31T00:00:00Z".to_string(),
            updated_at: format!("2026-05-31T00:{idx:02}:00Z"),
            last_user_message: Some(format!("message-{idx}")),
            last_assistant_message: None,
        }
    }

    fn start_turn(app: &mut App) {
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

    fn start_tool_call(app: &mut App, event_id: &str, call_id: &str, tool_name: &str, args: &str) {
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

    fn complete_tool_call(
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

    fn complete_agent_message(app: &mut App, event_id: &str, item_id: &str, text: &str) {
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

    fn reasoning_delta(app: &mut App, event_id: &str, item_id: &str, delta: &str) {
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

    fn complete_reasoning(app: &mut App, event_id: &str, item_id: &str, text: &str) {
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

    #[test]
    fn assistant_message_delta_complete_and_turn_complete_render_once() {
        let mut app = App::new();

        start_turn(&mut app);
        app.handle_session_event(
            event(
                "1",
                EventMsg::AgentMessageDelta(AgentMessageDeltaEvent {
                    thread_id: String::from("thread"),
                    turn_id: String::from("turn-1"),
                    item_id: String::from("assistant-1"),
                    delta: String::from("Hi! What can I help with?\n"),
                }),
            ),
            20,
        );
        complete_agent_message(&mut app, "2", "assistant-1", "Hi! What can I help with?");
        app.handle_session_event(
            event(
                "3",
                EventMsg::TurnCompleted(TurnCompletedEvent {
                    thread_id: String::from("thread"),
                    turn_id: String::from("turn-1"),
                    last_assistant_message: Some(String::from("Hi! What can I help with?")),
                }),
            ),
            20,
        );

        let joined = transcript_strings(&app).join("\n");
        assert_eq!(joined.matches("Hi! What can I help with?").count(), 1);
    }

    #[test]
    fn completed_assistant_message_followed_by_agent_message_renders_once() {
        let mut app = App::new();

        start_turn(&mut app);
        complete_agent_message(&mut app, "2", "assistant-1", "Done.");
        app.handle_session_event(
            event(
                "3",
                EventMsg::AgentMessage {
                    text: "Done.".to_string(),
                },
            ),
            20,
        );

        let joined = transcript_strings(&app).join("\n");
        assert_eq!(joined.matches("Done.").count(), 1);
    }

    #[test]
    fn assistant_delta_without_newline_renders_while_streaming() {
        let mut app = App::new();

        start_turn(&mut app);
        app.handle_session_event(
            event(
                "1",
                EventMsg::AgentMessageDelta(AgentMessageDeltaEvent {
                    thread_id: String::from("thread"),
                    turn_id: String::from("turn-1"),
                    item_id: String::from("assistant-1"),
                    delta: String::from("hello"),
                }),
            ),
            20,
        );

        assert_eq!(transcript_strings(&app), vec![String::from("• hello")]);
    }

    #[test]
    fn last_user_message_drops_its_bottom_separator() {
        let mut app = App::new();
        start_turn(&mut app);
        app.handle_session_event(
            event(
                "u1",
                EventMsg::UserMessage {
                    text: "hello".to_string(),
                },
            ),
            20,
        );

        let texts = transcript_strings(&app);
        let rule = "─".repeat(80);
        // Only the top rule is drawn; the bottom rule is dropped because the
        // user message is the last thing in the transcript.
        assert_eq!(texts.iter().filter(|t| **t == rule).count(), 1, "{texts:?}");
        assert_eq!(
            texts.last().map(String::as_str),
            Some("▌ hello"),
            "{texts:?}"
        );
    }

    #[test]
    fn user_message_keeps_bottom_separator_when_followed_by_reply() {
        let mut app = App::new();
        start_turn(&mut app);
        app.handle_session_event(
            event(
                "u1",
                EventMsg::UserMessage {
                    text: "hello".to_string(),
                },
            ),
            20,
        );
        complete_agent_message(&mut app, "a1", "assistant-1", "world");

        let texts = transcript_strings(&app);
        let rule = "─".repeat(80);
        // The user message now has a reply after it, so both rules are drawn.
        assert_eq!(texts.iter().filter(|t| **t == rule).count(), 2, "{texts:?}");
    }

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
    fn reasoning_delta_then_completed_renders_once() {
        let mut app = App::new();

        start_turn(&mut app);
        reasoning_delta(&mut app, "2", "reasoning-1", "Thinking through this...\n");
        complete_reasoning(&mut app, "3", "reasoning-1", "Thinking through this...");

        let transcript = transcript_strings(&app);
        let joined = transcript.join("\n");

        assert_eq!(joined.matches("Thinking through this...").count(), 1);
        assert!(
            transcript
                .iter()
                .any(|line| line == "… Thinking through this...")
        );
    }

    #[test]
    fn late_reasoning_completion_after_stream_finalized_via_assistant_does_not_duplicate() {
        let mut app = App::new();

        start_turn(&mut app);
        reasoning_delta(&mut app, "2", "r-1", "Planning\n");
        app.handle_session_event(
            event(
                "3",
                EventMsg::AgentMessageDelta(AgentMessageDeltaEvent {
                    thread_id: String::from("thread"),
                    turn_id: String::from("turn-1"),
                    item_id: String::from("a-1"),
                    delta: String::from("Done.\n"),
                }),
            ),
            20,
        );
        complete_agent_message(&mut app, "4", "a-1", "Done.");
        complete_reasoning(&mut app, "5", "r-1", "Planning");

        let joined = transcript_strings(&app).join("\n");
        assert_eq!(joined.matches("Planning").count(), 1);
    }

    #[test]
    fn tool_call_start_then_complete_renders_single_row() {
        let mut app = App::new();

        start_turn(&mut app);
        start_tool_call(&mut app, "2", "c1", "read", "foo.rs");
        complete_tool_call(&mut app, "3", "c1", true, None);

        let transcript = transcript_strings(&app);
        let joined = transcript.join("\n");

        assert_eq!(
            transcript
                .iter()
                .filter(|line| line.contains("read foo.rs"))
                .count(),
            1
        );
        assert!(joined.contains("✓ read foo.rs"));
        assert!(!joined.contains("BIG CONTENT"));
    }

    #[test]
    fn consecutive_same_tool_calls_render_as_group() {
        let mut app = App::new();

        start_turn(&mut app);
        start_tool_call(&mut app, "2", "c1", "read", "foo.rs");
        start_tool_call(&mut app, "3", "c2", "read", "bar.rs");
        complete_tool_call(&mut app, "4", "c1", true, None);
        complete_tool_call(&mut app, "5", "c2", true, None);

        let joined = transcript_strings(&app).join("\n");
        assert!(joined.contains("✓ read\n      ✓ foo.rs\n      ✓ bar.rs"));
        assert!(!joined.contains("✓ read foo.rs"));
    }

    #[test]
    fn reasoning_breaks_tool_group() {
        let mut app = App::new();

        start_turn(&mut app);
        start_tool_call(&mut app, "2", "c1", "read", "foo.rs");
        complete_tool_call(&mut app, "3", "c1", true, None);
        complete_reasoning(&mut app, "4", "reasoning-1", "Checking context");
        start_tool_call(&mut app, "5", "c2", "read", "bar.rs");
        complete_tool_call(&mut app, "6", "c2", true, None);

        assert_eq!(
            transcript_strings(&app),
            vec![
                String::from("✓ read foo.rs"),
                String::new(),
                String::from("… Checking context"),
                String::new(),
                String::from("✓ read bar.rs"),
            ]
        );
    }

    #[test]
    fn phantom_stream_breaks_group() {
        let mut app = App::new();

        start_turn(&mut app);
        start_tool_call(&mut app, "2", "c1", "read", "foo.rs");
        complete_tool_call(&mut app, "3", "c1", true, None);

        app.model.assistant_stream = Some(StreamController::new(Some(20)));

        start_tool_call(&mut app, "4", "c2", "read", "bar.rs");
        complete_tool_call(&mut app, "5", "c2", true, None);

        let transcript = transcript_strings(&app);
        assert!(transcript.iter().any(|line| line == "✓ read foo.rs"));
        assert!(transcript.iter().any(|line| line == "✓ read bar.rs"));
        assert!(!transcript.iter().any(|line| line == "✓ read"));
    }

    #[test]
    fn failed_entry_inside_group_shows_error_reason() {
        let mut app = App::new();

        start_turn(&mut app);
        start_tool_call(&mut app, "2", "c1", "read", "foo.rs");
        start_tool_call(&mut app, "3", "c2", "read", "bar.rs");
        complete_tool_call(&mut app, "4", "c1", true, None);
        complete_tool_call(&mut app, "5", "c2", false, Some("permission denied"));

        assert_eq!(
            transcript_strings(&app),
            vec![
                String::from("✗ read"),
                String::from("      ✓ foo.rs"),
                String::from("      ✗ bar.rs"),
                String::from("        ! permission denied"),
            ]
        );
    }

    #[test]
    fn file_change_completion_renders_diff_in_transcript() -> Result<(), Box<dyn std::error::Error>>
    {
        let mut app = App::new();

        start_turn(&mut app);
        start_tool_call(&mut app, "2", "c1", "edit", "src/lib.rs");
        app.handle_session_event(
            event(
                "3",
                EventMsg::ToolCallCompleted(ToolCallCompletedEvent {
                    thread_id: String::from("thread"),
                    turn_id: String::from("turn-1"),
                    call_id: String::from("c1"),
                    success: true,
                    output_preview: Some(String::from("edited src/lib.rs (1 replacement)")),
                    error: None,
                    result_kind: ToolCallResultKind::Final,
                    related_thread_id: None,
                    file_change: Some(smooth_protocol::FileChangeOutput {
                        path: "src/lib.rs".into(),
                        change: smooth_protocol::FileChange::Update {
                            unified_diff: diffy::create_patch("old\n", "new\n").to_string(),
                            move_path: None,
                        },
                    }),
                    file_changes: Vec::new(),
                    todos: Vec::new(),
                }),
            ),
            20,
        );

        let joined = transcript_strings(&app).join("\n");
        assert!(joined.contains("• Edited 1 file (+1 -1)"));
        assert!(joined.contains("src/lib.rs (+1 -1)"));
        assert!(joined.contains("1 - old"));
        assert!(joined.contains("1 + new"));
        assert!(!joined.contains("✓ edit src/lib.rs"));
        assert_eq!(app.model.recent_file_changes.len(), 1);

        app.model.screen = Screen::Workspace;
        app.model.focus = FocusTarget::Transcript;
        let mut terminal = Terminal::new(TestBackend::new(120, 24))?;
        terminal.draw(|frame| app.render(frame))?;
        let rendered = rendered_buffer_text(&terminal);
        assert!(rendered.contains("1 - old"), "{rendered}");
        assert!(rendered.contains("1 + new"), "{rendered}");
        assert!(!rendered.contains("Selected Diff"), "{rendered}");
        Ok(())
    }

    #[test]
    fn todo_write_completion_renders_checklist_in_transcript() {
        let mut app = App::new();

        start_turn(&mut app);
        start_tool_call(&mut app, "2", "c1", "todo_write", "{\"todos\":[...]}");
        app.handle_session_event(
            event(
                "3",
                EventMsg::ToolCallCompleted(ToolCallCompletedEvent {
                    thread_id: String::from("thread"),
                    turn_id: String::from("turn-1"),
                    call_id: String::from("c1"),
                    success: true,
                    output_preview: Some(String::from("Todo list updated: 2 items")),
                    error: None,
                    result_kind: ToolCallResultKind::Final,
                    related_thread_id: None,
                    file_change: None,
                    file_changes: Vec::new(),
                    todos: vec![
                        TodoItem {
                            content: String::from("add module"),
                            status: smooth_protocol::TodoStatus::Completed,
                        },
                        TodoItem {
                            content: String::from("register tool"),
                            status: smooth_protocol::TodoStatus::InProgress,
                        },
                    ],
                }),
            ),
            80,
        );

        let joined = transcript_strings(&app).join("\n");
        assert!(joined.contains("☑ add module"), "{joined}");
        assert!(joined.contains("◐ register tool"), "{joined}");
        assert!(!joined.contains("todo_write"), "{joined}");
    }

    #[test]
    fn move_file_change_renders_destination() -> Result<(), Box<dyn std::error::Error>> {
        let mut app = App::new();

        start_turn(&mut app);
        start_tool_call(&mut app, "2", "c1", "edit", "src/old.rs");
        app.handle_session_event(
            event(
                "3",
                EventMsg::ToolCallCompleted(ToolCallCompletedEvent {
                    thread_id: String::from("thread"),
                    turn_id: String::from("turn-1"),
                    call_id: String::from("c1"),
                    success: true,
                    output_preview: Some(String::from("applied edits (1 file changed)")),
                    error: None,
                    result_kind: ToolCallResultKind::Final,
                    related_thread_id: None,
                    file_change: Some(smooth_protocol::FileChangeOutput {
                        path: "src/old.rs".into(),
                        change: smooth_protocol::FileChange::Update {
                            unified_diff: String::new(),
                            move_path: Some("src/new.rs".into()),
                        },
                    }),
                    file_changes: Vec::new(),
                    todos: Vec::new(),
                }),
            ),
            20,
        );

        let joined = transcript_strings(&app).join("\n");
        assert!(joined.contains("• Moved 1 file (+0 -0)"));
        assert!(joined.contains("src/old.rs -> src/new.rs (+0 -0)"));

        app.model.screen = Screen::Workspace;
        app.model.focus = FocusTarget::Transcript;
        let mut terminal = Terminal::new(TestBackend::new(120, 24))?;
        terminal.draw(|frame| app.render(frame))?;
        let rendered = rendered_buffer_text(&terminal);
        assert!(rendered.contains("src/old.rs -> src/new.rs"), "{rendered}");
        Ok(())
    }

    #[test]
    fn multi_file_change_completion_renders_multiple_patch_items() {
        let mut app = App::new();

        start_turn(&mut app);
        start_tool_call(
            &mut app,
            "2",
            "c1",
            "edit",
            "{\"updates\":[{\"file_path\":\"one.txt\",\"hunks\":[]}]}",
        );
        app.handle_session_event(
            event(
                "3",
                EventMsg::ToolCallCompleted(ToolCallCompletedEvent {
                    thread_id: String::from("thread"),
                    turn_id: String::from("turn-1"),
                    call_id: String::from("c1"),
                    success: true,
                    output_preview: Some(String::from("applied edits (2 files changed)")),
                    error: None,
                    result_kind: ToolCallResultKind::Final,
                    related_thread_id: None,
                    file_change: None,
                    file_changes: vec![
                        smooth_protocol::FileChangeOutput {
                            path: "one.txt".into(),
                            change: smooth_protocol::FileChange::Update {
                                unified_diff: diffy::create_patch("one\n", "uno\n").to_string(),
                                move_path: None,
                            },
                        },
                        smooth_protocol::FileChangeOutput {
                            path: "two.txt".into(),
                            change: smooth_protocol::FileChange::Add {
                                content: "dos\n".to_string(),
                            },
                        },
                    ],
                    todos: Vec::new(),
                }),
            ),
            20,
        );

        let joined = transcript_strings(&app).join("\n");
        assert!(joined.contains("one.txt (+1 -1)"));
        assert!(joined.contains("two.txt (+1 -0)"));
        assert!(!joined.contains("✓ edit"));
        assert_eq!(app.model.recent_file_changes.len(), 2);
    }

    #[test]
    fn file_change_completion_auto_scrolls_after_summary_expands() {
        let mut app = App::new();
        let viewport_height = 1;

        start_turn(&mut app);
        start_tool_call(&mut app, "2", "c1", "write", "large.txt");
        app.handle_session_event(
            event(
                "3",
                EventMsg::ToolCallCompleted(ToolCallCompletedEvent {
                    thread_id: String::from("thread"),
                    turn_id: String::from("turn-1"),
                    call_id: String::from("c1"),
                    success: true,
                    output_preview: Some(String::from("wrote bytes to large.txt")),
                    error: None,
                    result_kind: ToolCallResultKind::Final,
                    related_thread_id: None,
                    file_change: Some(smooth_protocol::FileChangeOutput {
                        path: "large.txt".into(),
                        change: smooth_protocol::FileChange::Add {
                            content: (0..40)
                                .map(|idx| format!("line {idx}"))
                                .collect::<Vec<_>>()
                                .join("\n"),
                        },
                    }),
                    file_changes: Vec::new(),
                    todos: Vec::new(),
                }),
            ),
            viewport_height,
        );

        assert!(app.model.scroll > 0);
        let max_scroll = app.max_scroll(viewport_height);
        assert_eq!(app.model.scroll, max_scroll);
    }

    #[test]
    fn long_edit_diff_rows_do_not_hide_bottom_marker() -> Result<(), Box<dyn std::error::Error>> {
        let mut app = App::new();
        app.model.screen = Screen::Workspace;
        app.model.focus = FocusTarget::Transcript;
        app.model.inspector_visible = false;
        let old = (0..20)
            .map(|_| "a".repeat(80))
            .collect::<Vec<_>>()
            .join("\n");
        let new = (0..20)
            .map(|_| "b".repeat(80))
            .collect::<Vec<_>>()
            .join("\n");

        start_turn(&mut app);
        start_tool_call(&mut app, "2", "c1", "edit", "src/lib.rs");
        app.handle_session_event(
            event(
                "3",
                EventMsg::ToolCallCompleted(ToolCallCompletedEvent {
                    thread_id: String::from("thread"),
                    turn_id: String::from("turn-1"),
                    call_id: String::from("c1"),
                    success: true,
                    output_preview: Some(String::from("edited src/lib.rs")),
                    error: None,
                    result_kind: ToolCallResultKind::Final,
                    related_thread_id: None,
                    file_change: Some(smooth_protocol::FileChangeOutput {
                        path: "src/lib.rs".into(),
                        change: smooth_protocol::FileChange::Update {
                            unified_diff: diffy::create_patch(
                                &format!("{old}\n"),
                                &format!("{new}\n"),
                            )
                            .to_string(),
                            move_path: None,
                        },
                    }),
                    file_changes: Vec::new(),
                    todos: Vec::new(),
                }),
            ),
            8,
        );
        app.model.push_info("bottom marker");
        let viewport_height = app.model.transcript_viewport_height(40, 12);
        app.model.transcript_inner_width = 40;
        app.model.scroll_to_bottom(viewport_height);
        app.model.auto_scroll = false;

        let mut terminal = Terminal::new(TestBackend::new(40, 12))?;
        terminal.draw(|frame| app.render(frame))?;
        let rendered = rendered_buffer_text(&terminal);

        assert!(rendered.contains("bottom marker"), "{rendered}");
        Ok(())
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
                EventMsg::CollabAgentCompleted(smooth_protocol::CollabAgentCompletedEvent {
                    parent_thread_id: ThreadId::new(),
                    child_thread_id,
                    agent_path: smooth_protocol::AgentPath::try_from("/root/child")?,
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
    fn final_spawn_completion_records_related_thread_on_entry() {
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
                    output_preview: Some(String::from("{\"status\":\"completed\"}")),
                    error: None,
                    result_kind: ToolCallResultKind::Final,
                    related_thread_id: Some(child_thread_id),
                    file_change: None,
                    file_changes: Vec::new(),
                    todos: Vec::new(),
                }),
            ),
            20,
        );

        let group = app
            .model
            .transcript_items
            .iter()
            .find_map(|item| item.tool_group_cell())
            .expect("spawn tool row");
        assert_eq!(group.subagent_thread_id(), Some(child_thread_id));
        assert!(group.is_spawn_agent());

        let joined = transcript_strings(&app).join("\n");
        assert!(joined.contains("↳ subagent"), "{joined}");
    }

    #[test]
    fn gd_on_subagent_row_emits_one_preview_effect_and_preserves_selection() {
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

        let t0 = Instant::now();
        enter_select(&mut app.model, t0);
        assert_eq!(app.model.mode, UiMode::TranscriptSelect);
        let tool_row = app
            .model
            .transcript_items
            .iter()
            .position(|item| item.tool_group_cell().is_some())
            .expect("spawn tool row index");
        if let Some(state) = app.model.transcript_select.as_mut() {
            state.selected = tool_row;
        }

        let t = t0 + Duration::from_millis(300);
        let effects = app.model.handle_key_event_at(key(KeyCode::Char('g')), t);
        assert!(effects.is_empty(), "g alone arms the chord");
        let effects = app
            .model
            .handle_key_event_at(key(KeyCode::Char('d')), t + Duration::from_millis(100));

        let preview_targets: Vec<ThreadId> = effects
            .iter()
            .filter_map(|effect| match effect.kind {
                UiEffectKind::ThreadPreview { thread_id } => Some(thread_id),
                _ => None,
            })
            .collect();
        assert_eq!(preview_targets, vec![child_thread_id]);
        assert_eq!(app.model.mode, UiMode::TranscriptSelect);
        assert_eq!(
            app.model.transcript_select.map(|state| state.selected),
            Some(tool_row),
            "gd leaves the parent selection untouched"
        );
    }

    #[test]
    fn gd_on_plain_row_is_noop_with_status() {
        let mut model = select_model_with_items(3);
        let t0 = Instant::now();
        enter_select(&mut model, t0);

        let t = t0 + Duration::from_millis(300);
        let _ = model.handle_key_event_at(key(KeyCode::Char('g')), t);
        let effects =
            model.handle_key_event_at(key(KeyCode::Char('d')), t + Duration::from_millis(100));

        assert!(effects.is_empty());
        assert_eq!(
            model.status_line,
            "Not a subagent row (gd opens spawn_agent sessions)"
        );
        assert_eq!(model.mode, UiMode::TranscriptSelect);
    }

    #[test]
    fn gd_on_spawn_row_without_child_id_reports_pending() {
        let mut app = App::new();
        start_turn(&mut app);
        start_tool_call(
            &mut app,
            "2",
            "c1",
            "spawn_agent",
            "{\"description\":\"inspect\",\"prompt\":\"inspect\"}",
        );

        let t0 = Instant::now();
        enter_select(&mut app.model, t0);
        let tool_row = app
            .model
            .transcript_items
            .iter()
            .position(|item| item.tool_group_cell().is_some())
            .expect("spawn tool row index");
        if let Some(state) = app.model.transcript_select.as_mut() {
            state.selected = tool_row;
        }

        let t = t0 + Duration::from_millis(300);
        let _ = app.model.handle_key_event_at(key(KeyCode::Char('g')), t);
        let effects = app
            .model
            .handle_key_event_at(key(KeyCode::Char('d')), t + Duration::from_millis(100));

        assert!(effects.is_empty());
        assert_eq!(
            app.model.status_line,
            "Subagent not started yet — no session to open"
        );
    }

    #[test]
    fn gd_on_spawn_row_after_spawn_end_opens_before_tool_result() {
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
                EventMsg::CollabAgentSpawnEnd(CollabAgentSpawnEndEvent {
                    call_id: String::from("c1"),
                    sender_thread_id: ThreadId::new(),
                    new_thread_id: Some(child_thread_id),
                    new_agent_nickname: Some(String::from("child")),
                    prompt: String::from("inspect"),
                    model: None,
                    status: AgentStatus::Running,
                }),
            ),
            20,
        );

        let t0 = Instant::now();
        enter_select(&mut app.model, t0);
        let Some(tool_row) = app
            .model
            .transcript_items
            .iter()
            .position(|item| item.tool_group_cell().is_some())
        else {
            panic!("spawn tool row index");
        };
        if let Some(state) = app.model.transcript_select.as_mut() {
            state.selected = tool_row;
        }

        let t = t0 + Duration::from_millis(300);
        let _ = app.model.handle_key_event_at(key(KeyCode::Char('g')), t);
        let effects = app
            .model
            .handle_key_event_at(key(KeyCode::Char('d')), t + Duration::from_millis(100));

        let preview_targets: Vec<ThreadId> = effects
            .iter()
            .filter_map(|effect| match effect.kind {
                UiEffectKind::ThreadPreview { thread_id } => Some(thread_id),
                _ => None,
            })
            .collect();
        assert_eq!(preview_targets, vec![child_thread_id]);
        assert_eq!(app.model.status_line, "Opening subagent…");
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
    fn failed_turn_start_restores_empty_composer_draft() {
        let mut model = UiModel::new();
        let thread_id = ThreadId::new();
        model.current_thread_id = Some(thread_id);
        model.screen = Screen::Workspace;
        model.composer.set_text("draft prompt".to_string());

        let effects = model.request_turn_start();

        assert_eq!(effects.len(), 1);
        assert!(model.composer.is_empty());
        assert_eq!(model.composer.cursor(), 0);

        let _ = model.update(UiEvent::EffectFailed {
            effect_id: effects[0].effect_id,
            error: "temporary failure".to_string(),
            viewport_height: 20,
        });

        assert_eq!(model.composer.as_str(), "draft prompt");
        assert_eq!(model.composer.cursor(), "draft prompt".len());
        assert_eq!(model.mode, UiMode::Insert);
        assert_eq!(model.focus, FocusTarget::Composer);
    }

    #[test]
    fn failed_turn_start_does_not_overwrite_new_composer_text() {
        let mut model = UiModel::new();
        let thread_id = ThreadId::new();
        model.current_thread_id = Some(thread_id);
        model.screen = Screen::Workspace;
        model.composer.set_text("old draft".to_string());

        let effects = model.request_turn_start();
        model.composer.set_text("new draft".to_string());
        let _ = model.update(UiEvent::EffectFailed {
            effect_id: effects[0].effect_id,
            error: "temporary failure".to_string(),
            viewport_height: 20,
        });

        assert_eq!(model.composer.as_str(), "new draft");
    }

    #[test]
    fn insert_mode_paste_inserts_at_cursor_and_normalizes_newlines() {
        let mut model = workspace_insert_model();
        model.composer.set_text("ab".to_string());

        let _ = model.handle_key_event(key(KeyCode::Left));
        let effects = model.handle_terminal_event(CrosstermEvent::Paste("X\r\nY\rZ".to_string()));

        assert!(effects.is_empty());
        assert_eq!(model.composer.as_str(), "aX\nY\nZb");
        assert_eq!(model.composer.cursor(), "aX\nY\nZ".len());
    }

    #[test]
    fn paste_while_other_editing_inserts_into_answer() {
        let mut model = UiModel::new();
        let thread_id = ThreadId::new();
        model.current_thread_id = Some(thread_id);
        let _ = model.update(UiEvent::ServerRequest(ServerRequest::AskUserQuestion {
            request_id: RequestId(42),
            params: AskUserQuestionParams {
                thread_id: thread_id.to_string(),
                turn_id: "turn".to_string(),
                questions: vec![AskUserQuestion {
                    question: "Pick a path?".to_string(),
                    header: "Choice".to_string(),
                    options: vec![
                        AskUserQuestionOption {
                            label: "A".to_string(),
                            description: "Use option A".to_string(),
                            preview: None,
                        },
                        AskUserQuestionOption {
                            label: "B".to_string(),
                            description: "Use option B".to_string(),
                            preview: None,
                        },
                    ],
                    multi_select: false,
                }],
            },
        }));

        // Move to the "Other" row and start editing, then paste.
        let _ = model.handle_key_event(key(KeyCode::Down));
        let _ = model.handle_key_event(key(KeyCode::Down));
        let _ = model.handle_key_event(key(KeyCode::Enter));
        let _ = model.handle_terminal_event(CrosstermEvent::Paste("pasted\nanswer".to_string()));
        let effects = model.handle_key_event(key(KeyCode::Enter));

        assert!(matches!(
            &effects[0].kind,
            UiEffectKind::AnswerQuestion { request_id, response }
                if *request_id == RequestId(42)
                    && response.answers[0].selected == vec!["pasted answer".to_string()]
        ));
    }

    #[test]
    fn insert_mode_left_and_right_edit_inside_composer() {
        let mut model = workspace_insert_model();
        model.composer.set_text("helo".to_string());

        let _ = model.handle_key_event(key(KeyCode::Left));
        let _ = model.handle_key_event(key(KeyCode::Char('l')));
        let _ = model.handle_key_event(key(KeyCode::Right));
        let _ = model.handle_key_event(key(KeyCode::Char('!')));

        assert_eq!(model.composer.as_str(), "hello!");
        assert_eq!(model.composer.cursor(), "hello!".len());
    }

    #[test]
    fn insert_mode_backspace_and_delete_edit_around_cursor() {
        let mut model = workspace_insert_model();
        model.composer.set_text("abc".to_string());

        let _ = model.handle_key_event(key(KeyCode::Left));
        let _ = model.handle_key_event(key(KeyCode::Backspace));
        assert_eq!(model.composer.as_str(), "ac");
        assert_eq!(model.composer.cursor(), "a".len());

        let _ = model.handle_key_event(key(KeyCode::Delete));
        assert_eq!(model.composer.as_str(), "a");
        assert_eq!(model.composer.cursor(), "a".len());
    }

    #[test]
    fn insert_mode_home_and_end_move_within_current_line() {
        let mut model = workspace_insert_model();
        model.composer.set_text("abc\ndef".to_string());

        let _ = model.handle_key_event(key(KeyCode::Home));
        assert_eq!(model.composer.cursor(), "abc\n".len());

        let _ = model.handle_key_event(key(KeyCode::End));
        assert_eq!(model.composer.cursor(), "abc\ndef".len());
    }

    #[test]
    fn insert_mode_up_and_down_preserve_visual_column() {
        let mut model = workspace_insert_model();
        model.composer.set_text("abcdef\nx\nabcdef".to_string());

        let _ = model.handle_key_event(key(KeyCode::Up));
        assert_eq!(model.composer.cursor(), "abcdef\nx".len());

        let _ = model.handle_key_event(key(KeyCode::Up));
        assert_eq!(model.composer.cursor(), "abcdef".len());

        let _ = model.handle_key_event(key(KeyCode::Down));
        assert_eq!(model.composer.cursor(), "abcdef\nx".len());

        let _ = model.handle_key_event(key(KeyCode::Down));
        assert_eq!(model.composer.cursor(), "abcdef\nx\nabcdef".len());
    }

    #[test]
    fn insert_mode_up_and_down_use_wrapped_visual_rows() {
        let mut model = workspace_insert_model();
        model.terminal_width = 5;
        model.composer.set_text("abcdef".to_string());

        let _ = model.handle_key_event(key(KeyCode::Up));

        assert_eq!(model.composer.cursor(), "a".len());
    }

    #[test]
    fn ctrl_enter_sends_full_composer_text_and_clears_cursor_state() {
        let mut model = workspace_insert_model();
        let thread_id = ThreadId::new();
        model.current_thread_id = Some(thread_id);
        model.composer.set_text("hello\nworld".to_string());

        let effects = model.handle_key_event(modified_key(KeyCode::Enter, KeyModifiers::CONTROL));

        assert_eq!(effects.len(), 1);
        assert!(matches!(
            &effects[0].kind,
            UiEffectKind::TurnStart {
                thread_id: got,
                input,
            } if *got == thread_id && input == "hello\nworld"
        ));
        assert!(model.composer.is_empty());
        assert_eq!(model.composer.cursor(), 0);
        assert_eq!(model.mode, UiMode::Normal);
        assert_eq!(model.focus, FocusTarget::Transcript);
    }

    fn skills_fixture() -> Result<(tempfile::TempDir, UiModel), Box<dyn std::error::Error>> {
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

    #[test]
    fn typing_slash_opens_skill_popup_and_filters() -> Result<(), Box<dyn std::error::Error>> {
        let (_temp, mut model) = skills_fixture()?;

        let _ = model.handle_key_event(key(KeyCode::Char('/')));
        let Some(popup) = model.skill_popup.as_ref() else {
            panic!("expected popup to open on '/'");
        };
        assert_eq!(popup.selected_name(), Some("deploy"));

        let _ = model.handle_key_event(key(KeyCode::Char('r')));
        let Some(popup) = model.skill_popup.as_ref() else {
            panic!("expected popup to stay open while filtering");
        };
        assert_eq!(popup.selected_name(), Some("review"));

        // No match closes the popup; backspacing back into a match reopens it.
        let _ = model.handle_key_event(key(KeyCode::Char('z')));
        assert!(model.skill_popup.is_none());
        let _ = model.handle_key_event(key(KeyCode::Backspace));
        assert!(model.skill_popup.is_some());
        Ok(())
    }

    #[test]
    fn slash_without_skills_does_not_open_popup() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::TempDir::new()?;
        let mut model = workspace_insert_model();
        model.skills_root = temp.path().to_path_buf();

        let _ = model.handle_key_event(key(KeyCode::Char('/')));
        assert!(model.skill_popup.is_none());
        Ok(())
    }

    #[test]
    fn slash_mid_text_does_not_open_popup() -> Result<(), Box<dyn std::error::Error>> {
        let (_temp, mut model) = skills_fixture()?;
        for ch in "run /".chars() {
            let _ = model.handle_key_event(key(KeyCode::Char(ch)));
        }
        assert!(model.skill_popup.is_none());
        Ok(())
    }

    #[test]
    fn tab_accepts_selected_skill_completion() -> Result<(), Box<dyn std::error::Error>> {
        let (_temp, mut model) = skills_fixture()?;

        let _ = model.handle_key_event(key(KeyCode::Char('/')));
        let _ = model.handle_key_event(key(KeyCode::Down));
        let _ = model.handle_key_event(key(KeyCode::Tab));

        assert_eq!(model.composer.as_str(), "/review ");
        assert!(model.skill_popup.is_none());
        assert_eq!(model.mode, UiMode::Insert);
        Ok(())
    }

    #[test]
    fn enter_accepts_skill_completion_instead_of_newline() -> Result<(), Box<dyn std::error::Error>>
    {
        let (_temp, mut model) = skills_fixture()?;

        for ch in "/dep".chars() {
            let _ = model.handle_key_event(key(KeyCode::Char(ch)));
        }
        let _ = model.handle_key_event(key(KeyCode::Enter));

        assert_eq!(model.composer.as_str(), "/deploy ");
        assert!(model.skill_popup.is_none());
        Ok(())
    }

    #[test]
    fn esc_closes_skill_popup_but_stays_in_insert_mode() -> Result<(), Box<dyn std::error::Error>> {
        let (_temp, mut model) = skills_fixture()?;

        let _ = model.handle_key_event(key(KeyCode::Char('/')));
        assert!(model.skill_popup.is_some());

        let _ = model.handle_key_event(key(KeyCode::Esc));
        assert!(model.skill_popup.is_none());
        assert_eq!(model.mode, UiMode::Insert);
        assert_eq!(model.composer.as_str(), "/");

        // A second Esc leaves Insert mode as usual.
        let _ = model.handle_key_event(key(KeyCode::Esc));
        assert_eq!(model.mode, UiMode::Normal);
        Ok(())
    }

    #[test]
    fn ctrl_enter_with_skill_popup_open_still_submits() -> Result<(), Box<dyn std::error::Error>> {
        let (_temp, mut model) = skills_fixture()?;
        let thread_id = ThreadId::new();
        model.current_thread_id = Some(thread_id);

        for ch in "/deploy".chars() {
            let _ = model.handle_key_event(key(KeyCode::Char(ch)));
        }
        assert!(model.skill_popup.is_some());
        let effects = model.handle_key_event(modified_key(KeyCode::Enter, KeyModifiers::CONTROL));

        assert_eq!(effects.len(), 1);
        assert!(matches!(
            &effects[0].kind,
            UiEffectKind::TurnStart { input, .. } if input == "/deploy"
        ));
        assert!(model.skill_popup.is_none());
        assert_eq!(model.mode, UiMode::Normal);
        assert_eq!(model.focus, FocusTarget::Transcript);
        Ok(())
    }

    #[test]
    fn skill_popup_renders_above_composer() -> Result<(), Box<dyn std::error::Error>> {
        let (_temp, mut model) = skills_fixture()?;
        let _ = model.handle_key_event(key(KeyCode::Char('/')));

        let mut terminal = Terminal::new(TestBackend::new(80, 18))?;
        terminal.draw(|frame| model.render(frame))?;

        let rendered = rendered_buffer_text(&terminal);
        let Some(deploy_idx) = rendered.find("/deploy  Deploy the app") else {
            panic!("popup row missing:\n{rendered}");
        };
        let Some(review_idx) = rendered.find("/review  Review a PR") else {
            panic!("popup row missing:\n{rendered}");
        };
        let Some(status_idx) = rendered.find("─ Status ") else {
            panic!("status separator missing:\n{rendered}");
        };
        assert!(deploy_idx < review_idx, "{rendered}");
        assert!(review_idx < status_idx, "{rendered}");
        Ok(())
    }

    #[test]
    fn super_enter_inserts_newline() {
        let mut model = workspace_insert_model();
        let thread_id = ThreadId::new();
        model.current_thread_id = Some(thread_id);
        model.composer.set_text("hello".to_string());

        let effects = model.handle_key_event(modified_key(KeyCode::Enter, KeyModifiers::SUPER));

        assert!(effects.is_empty());
        assert_eq!(model.composer.as_str(), "hello\n");
    }

    #[test]
    fn visible_transcript_lines_at_bottom_are_exact_viewport_rows() {
        let mut model = UiModel::new();
        for idx in 1..=5 {
            model.push_info(format!("line {idx}"));
        }
        model.terminal_width = 80;
        model.scroll_to_bottom(3);

        let lines = model.visible_transcript_lines(78, 3);
        let rendered = lines
            .into_iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();

        assert_eq!(rendered.len(), 3);
        assert_eq!(rendered.last().map(String::as_str), Some("i line 5"));
    }

    #[test]
    fn rendered_transcript_bottom_includes_newest_row() -> Result<(), Box<dyn std::error::Error>> {
        let mut model = UiModel::new();
        model.screen = Screen::Workspace;
        model.focus = FocusTarget::Transcript;
        for idx in 1..=12 {
            model.push_info(format!("line {idx}"));
        }
        model.terminal_width = 50;
        let viewport_height = model.transcript_viewport_height(50, 10);
        model.scroll = model.max_scroll(viewport_height);
        model.auto_scroll = false;

        let mut terminal = Terminal::new(TestBackend::new(50, 10))?;
        terminal.draw(|frame| model.render(frame))?;

        let rendered = rendered_buffer_text(&terminal);
        assert!(rendered.contains("i line 12"), "{rendered}");
        Ok(())
    }

    #[test]
    fn split_view_auto_scroll_reaches_wrapping_bottom() -> Result<(), Box<dyn std::error::Error>> {
        let mut model = UiModel::new();
        model.screen = Screen::Workspace;
        model.focus = FocusTarget::Transcript;
        // A wide terminal (>= 110) enables the split workspace, so the transcript
        // pane is only ~70% of the width. These lines wrap at the pane width but
        // would not wrap at the full terminal width — the case where row counting
        // at the wrong width left the newest content unreachable.
        for idx in 1..=20 {
            model.push_info(format!("{} marker{idx}", "x".repeat(90)));
        }
        model.auto_scroll = true;

        let mut terminal = Terminal::new(TestBackend::new(120, 24))?;
        terminal.draw(|frame| model.render(frame))?;

        let rendered = rendered_buffer_text(&terminal);
        assert!(
            rendered.contains("marker20"),
            "newest content missing:\n{rendered}"
        );
        Ok(())
    }

    #[test]
    fn active_wrap_is_cached_by_width_and_version() {
        let mut model = UiModel::new();
        model.set_active_assistant_lines(Some(vec![Line::raw("streaming text here")]));

        model.refresh_active_wrap(40);
        assert_eq!(model.active_wrap_computes, 1);

        // Same width and unchanged version: cache hit, no recompute.
        model.refresh_active_wrap(40);
        assert_eq!(model.active_wrap_computes, 1);

        // A different width is a miss.
        model.refresh_active_wrap(20);
        assert_eq!(model.active_wrap_computes, 2);

        // A new delta bumps the version, so even the same width recomputes.
        model.set_active_assistant_lines(Some(vec![Line::raw("streaming text and more")]));
        model.refresh_active_wrap(20);
        assert_eq!(model.active_wrap_computes, 3);
    }

    #[test]
    fn visible_transcript_lines_counts_active_separator_above_viewport() {
        let mut model = UiModel::new();
        model.push_info("history");
        model.set_active_assistant_lines(Some(vec![
            Line::raw("active one"),
            Line::raw("active two"),
            Line::raw("active three"),
        ]));
        model.terminal_width = 80;
        model.scroll_to_bottom(3);

        let rendered = model
            .visible_transcript_lines(78, 3)
            .into_iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();

        assert_eq!(
            rendered,
            vec![
                "• active one".to_string(),
                "  active two".to_string(),
                "  active three".to_string(),
            ]
        );
    }

    #[test]
    fn long_tool_call_rows_are_counted_after_wrapping() {
        let mut model = UiModel::new();
        let item_id = model.next_item_id();
        model.push_history(TranscriptItem::tool_group(
            item_id,
            ToolCallGroupCell::new("read".to_string(), "x".repeat(80)),
        ));

        let rendered = model.visible_transcript_lines(20, 20);
        let total_rows = model.total_transcript_rows(20);

        assert!(rendered.len() > 1);
        assert_eq!(total_rows, rendered.len());
    }

    #[test]
    fn dashboard_thread_start_and_resume_failures_are_visible() {
        let mut model = UiModel::new();
        let start_effect = model.effect(EffectContext::ThreadStart, UiEffectKind::ThreadStart);

        let _ = model.update(UiEvent::EffectFailed {
            effect_id: start_effect.effect_id,
            error: "server down".to_string(),
            viewport_height: 20,
        });

        assert_eq!(
            model.dashboard.error.as_deref(),
            Some("could not start thread: server down")
        );
        assert_eq!(model.screen, Screen::Dashboard);

        let thread_id = ThreadId::new();
        let resume_effect = model.effect(
            EffectContext::ThreadResume { thread_id },
            UiEffectKind::ThreadResume { thread_id },
        );
        let _ = model.update(UiEvent::EffectFailed {
            effect_id: resume_effect.effect_id,
            error: "missing".to_string(),
            viewport_height: 20,
        });

        assert_eq!(
            model.dashboard.error.as_deref(),
            Some(format!("could not resume thread {thread_id}: missing").as_str())
        );
        assert_eq!(model.screen, Screen::Dashboard);
    }

    #[test]
    fn plan_mode_effect_is_optimistic_and_failure_reverts() {
        let mut model = UiModel::new();
        let thread_id = ThreadId::new();
        model.current_thread_id = Some(thread_id);

        let effects = model.execute_command("plan");

        assert_eq!(effects.len(), 1);
        assert!(model.plan_mode);
        assert_eq!(model.effect_contexts.len(), 1);

        let _ = model.update(UiEvent::EffectFailed {
            effect_id: effects[0].effect_id,
            error: "nope".to_string(),
            viewport_height: 20,
        });

        assert!(!model.plan_mode);
        assert!(model.effect_contexts.is_empty());
        assert!(
            model
                .transcript_lines_uncached(80)
                .join("\n")
                .contains("could not enable plan mode")
        );
    }

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
    fn hidden_inspector_is_skipped_by_tab_and_backtab() {
        let mut model = workspace_normal_model();
        model.inspector_visible = false;

        let _ = model.handle_key_event(key(KeyCode::Tab));
        assert_eq!(model.focus, FocusTarget::Composer);

        let _ = model.handle_key_event(key(KeyCode::Tab));
        assert_eq!(model.focus, FocusTarget::Transcript);

        let _ = model.handle_key_event(key(KeyCode::BackTab));
        assert_eq!(model.focus, FocusTarget::Composer);

        let _ = model.handle_key_event(key(KeyCode::BackTab));
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
    fn workspace_renders_input_above_status_footer() -> Result<(), Box<dyn std::error::Error>> {
        let mut model = workspace_insert_model();
        model.composer.set_text("draft".to_string());

        let mut terminal = Terminal::new(TestBackend::new(80, 16))?;
        terminal.draw(|frame| model.render(frame))?;

        let rendered = rendered_buffer_text(&terminal);
        let Some(input_idx) = rendered.find("─ Input ") else {
            panic!("input separator missing:\n{rendered}");
        };
        let Some(status_idx) = rendered.find("─ Status ") else {
            panic!("status separator missing:\n{rendered}");
        };
        assert!(input_idx < status_idx, "{rendered}");
        Ok(())
    }

    #[test]
    fn command_line_renders_above_status_footer() -> Result<(), Box<dyn std::error::Error>> {
        let mut model = workspace_normal_model();
        model.mode = UiMode::Command;
        model.command = "help".to_string();
        model
            .composer
            .set_text("first input row\nsecond input row".to_string());

        let mut terminal = Terminal::new(TestBackend::new(80, 18))?;
        terminal.draw(|frame| model.render(frame))?;

        let rendered = rendered_buffer_text(&terminal);
        let Some(first_input_idx) = rendered.find("first input row") else {
            panic!("composer text missing:\n{rendered}");
        };
        let Some(second_input_idx) = rendered.find("second input row") else {
            panic!("multi-line composer text missing:\n{rendered}");
        };
        let Some(command_idx) = rendered.find(":help") else {
            panic!("command line missing:\n{rendered}");
        };
        let Some(status_idx) = rendered.find("─ Status ") else {
            panic!("status separator missing:\n{rendered}");
        };

        assert!(first_input_idx < second_input_idx, "{rendered}");
        assert!(second_input_idx < command_idx, "{rendered}");
        assert!(command_idx < status_idx, "{rendered}");
        Ok(())
    }

    #[test]
    fn hidden_inspector_renders_transcript_across_full_workspace_body()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut model = workspace_normal_model();
        model.inspector_visible = false;
        model.push_info("body marker");

        let mut terminal = Terminal::new(TestBackend::new(120, 24))?;
        terminal.draw(|frame| model.render(frame))?;

        let rendered = rendered_buffer_text(&terminal);
        assert!(!rendered.contains("Inspector"), "{rendered}");
        assert!(rendered.contains("body marker"), "{rendered}");
        assert_eq!(model.transcript_inner_width, 120);
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
    fn turn_start_effect_before_and_after_protocol_yields_one_running_turn() {
        let mut model = UiModel::new();
        let thread_id = ThreadId::new();
        let response = TurnStartResponse {
            thread_id: thread_id.to_string(),
            turn_id: "turn-1".to_string(),
        };

        let _ = model.update(UiEvent::EffectCompleted {
            effect_id: EffectId(1),
            result: UiEffectResult::TurnStart(response.clone()),
            viewport_height: 20,
        });
        model.apply_protocol_event(event(
            "turn-start",
            EventMsg::TurnStarted(TurnStartedEvent {
                thread_id: thread_id.to_string(),
                turn_id: "turn-1".to_string(),
            }),
        ));
        let _ = model.update(UiEvent::EffectCompleted {
            effect_id: EffectId(2),
            result: UiEffectResult::TurnStart(response),
            viewport_height: 20,
        });

        assert!(model.is_turn_running);
        assert_eq!(model.current_turn_id.as_deref(), Some("turn-1"));
    }

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
    fn active_ask_user_request_switches_to_workspace_overlay()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut model = UiModel::new();
        let thread_id = ThreadId::new();
        model.current_thread_id = Some(thread_id);
        let _ = model.update(UiEvent::ServerRequest(ServerRequest::AskUserQuestion {
            request_id: RequestId(42),
            params: AskUserQuestionParams {
                thread_id: thread_id.to_string(),
                turn_id: "turn".to_string(),
                questions: vec![AskUserQuestion {
                    question: "Pick a path?".to_string(),
                    header: "Choice".to_string(),
                    options: vec![AskUserQuestionOption {
                        label: "A".to_string(),
                        description: "Use option A".to_string(),
                        preview: None,
                    }],
                    multi_select: false,
                }],
            },
        }));

        assert_eq!(model.screen, Screen::Workspace);
        assert_eq!(model.mode, UiMode::Overlay);

        let mut terminal = Terminal::new(TestBackend::new(80, 24))?;
        terminal.draw(|frame| model.render(frame))?;
        let rendered = rendered_buffer_text(&terminal);

        assert!(rendered.contains("Pick a path?"), "{rendered}");
        assert!(rendered.contains("Use option A"), "{rendered}");
        Ok(())
    }

    #[test]
    fn confirming_question_picker_pushes_answer_summary_row()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut model = UiModel::new();
        let thread_id = ThreadId::new();
        model.current_thread_id = Some(thread_id);
        let _ = model.update(UiEvent::ServerRequest(ServerRequest::AskUserQuestion {
            request_id: RequestId(42),
            params: AskUserQuestionParams {
                thread_id: thread_id.to_string(),
                turn_id: "turn".to_string(),
                questions: vec![AskUserQuestion {
                    question: "Pick a path?".to_string(),
                    header: "Choice".to_string(),
                    options: vec![
                        AskUserQuestionOption {
                            label: "A".to_string(),
                            description: "Use option A".to_string(),
                            preview: None,
                        },
                        AskUserQuestionOption {
                            label: "B".to_string(),
                            description: "Use option B".to_string(),
                            preview: None,
                        },
                    ],
                    multi_select: false,
                }],
            },
        }));

        let effects = model.handle_key_event(key(KeyCode::Enter));

        assert!(model.question_picker.is_none());
        assert!(matches!(
            &effects[0].kind,
            UiEffectKind::AnswerQuestion { request_id, .. } if *request_id == RequestId(42)
        ));

        let mut terminal = Terminal::new(TestBackend::new(80, 24))?;
        terminal.draw(|frame| model.render(frame))?;
        let rendered = rendered_buffer_text(&terminal);
        assert!(rendered.contains("? Pick a path?"), "{rendered}");
        assert!(rendered.contains("→ A"), "{rendered}");
        Ok(())
    }

    #[test]
    fn dashboard_does_not_render_question_picker_overlay() -> Result<(), Box<dyn std::error::Error>>
    {
        let mut model = UiModel::new();
        model.question_picker = Some(QuestionPicker::new(
            RequestId(42),
            AskUserQuestionParams {
                thread_id: "thread".to_string(),
                turn_id: "turn".to_string(),
                questions: vec![AskUserQuestion {
                    question: "Pick a path?".to_string(),
                    header: "Choice".to_string(),
                    options: vec![AskUserQuestionOption {
                        label: "A".to_string(),
                        description: "Use option A".to_string(),
                        preview: None,
                    }],
                    multi_select: false,
                }],
            },
        ));
        model.screen = Screen::Dashboard;

        let mut terminal = Terminal::new(TestBackend::new(80, 24))?;
        terminal.draw(|frame| model.render(frame))?;
        let rendered = rendered_buffer_text(&terminal);

        assert!(!rendered.contains("Pick a path?"), "{rendered}");
        assert!(!rendered.contains("Use option A"), "{rendered}");
        assert!(rendered.contains("smooth-code"), "{rendered}");
        Ok(())
    }

    #[test]
    fn dashboard_keys_do_not_dispatch_hidden_question_picker() {
        let mut model = UiModel::new();
        let thread_id = ThreadId::new();
        model.dashboard.items = vec![ThreadListItem {
            thread_id: thread_id.to_string(),
            rollout_path: "session.jsonl".to_string(),
            created_at: "2026-05-31T00:00:00Z".to_string(),
            updated_at: "2026-05-31T00:01:00Z".to_string(),
            last_user_message: Some("hello".to_string()),
            last_assistant_message: None,
        }];
        model.question_picker = Some(QuestionPicker::new(
            RequestId(42),
            AskUserQuestionParams {
                thread_id: thread_id.to_string(),
                turn_id: "turn".to_string(),
                questions: vec![AskUserQuestion {
                    question: "Pick a path?".to_string(),
                    header: "Choice".to_string(),
                    options: vec![AskUserQuestionOption {
                        label: "A".to_string(),
                        description: "Use option A".to_string(),
                        preview: None,
                    }],
                    multi_select: false,
                }],
            },
        ));
        model.screen = Screen::Dashboard;

        let effects = model.handle_key_event(key(KeyCode::Enter));

        assert_eq!(effects.len(), 1);
        assert!(matches!(
            effects[0].kind,
            UiEffectKind::ThreadResume { thread_id: got } if got == thread_id
        ));
        assert!(model.question_picker.is_some());
    }

    #[test]
    fn ask_user_request_from_inactive_thread_is_failed_without_picker() {
        let mut model = UiModel::new();
        let active_thread = ThreadId::new();
        let stale_thread = ThreadId::new();
        model.current_thread_id = Some(active_thread);

        let effects = model.update(UiEvent::ServerRequest(ServerRequest::AskUserQuestion {
            request_id: RequestId(43),
            params: AskUserQuestionParams {
                thread_id: stale_thread.to_string(),
                turn_id: "turn".to_string(),
                questions: vec![AskUserQuestion {
                    question: "Pick a path?".to_string(),
                    header: "Choice".to_string(),
                    options: vec![AskUserQuestionOption {
                        label: "A".to_string(),
                        description: "Use option A".to_string(),
                        preview: None,
                    }],
                    multi_select: false,
                }],
            },
        }));

        assert_eq!(effects.len(), 1);
        assert!(model.question_picker.is_none());
        assert_ne!(model.mode, UiMode::Overlay);
        assert!(matches!(
            &effects[0].kind,
            UiEffectKind::FailServerRequest { request_id, error }
                if *request_id == RequestId(43)
                    && error.message.contains("inactive thread")
        ));
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

    #[test]
    fn dashboard_render_shows_only_visible_scrolled_sessions()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut model = UiModel::new();
        model.viewport_height = 8;
        model.dashboard.items = (0..8).map(dashboard_thread).collect();
        model.dashboard.selected = 3;
        model.dashboard_ensure_selected_visible(8);

        let mut terminal = Terminal::new(TestBackend::new(80, 8))?;
        terminal.draw(|frame| model.render(frame))?;
        let rendered = rendered_buffer_text(&terminal);

        assert!(!rendered.contains("message-0"), "{rendered}");
        assert!(rendered.contains("message-2"), "{rendered}");
        assert!(
            rendered.contains("› 2026-05-31T00:03:00Z  message-3"),
            "{rendered}"
        );
        assert!(!rendered.contains("message-4"), "{rendered}");
        Ok(())
    }

    #[test]
    fn dashboard_truncates_long_previews_and_keeps_footer_visible()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut model = UiModel::new();
        let items: Vec<ThreadListItem> = (0..3)
            .map(|idx| {
                let mut item = dashboard_thread(idx);
                item.last_user_message = Some(format!("preview-{idx} ").repeat(40));
                item
            })
            .collect();
        model.dashboard.items = items;

        // header (2) + 3 sessions * 2 + footer (2) == 10 rows: only fits if the
        // long previews are truncated to one row each rather than wrapped.
        let width = 50usize;
        let mut terminal = Terminal::new(TestBackend::new(width as u16, 10))?;
        terminal.draw(|frame| model.render(frame))?;
        let rows = buffer_rows(&terminal, width);

        assert!(
            rows.iter().any(|r| r.contains("n new")),
            "footer clipped by wrapped previews: {rows:?}"
        );
        assert!(
            rows.iter().any(|r| r.contains('…')),
            "preview was not truncated: {rows:?}"
        );
        let id_rows = rows.iter().filter(|r| r.contains("thread-")).count();
        assert_eq!(id_rows, 3, "each session should stay two rows: {rows:?}");
        Ok(())
    }

    #[test]
    fn inspector_wraps_long_running_tool_args_with_hanging_indent()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut model = workspace_normal_model();
        model.inspector_visible = true;
        model.focus = FocusTarget::Inspector;
        model.running_tools.insert(
            "call-1".to_string(),
            RunningToolInfo {
                tool_name: "run_command".to_string(),
                args_preview: "echo this is a really long command preview that must wrap"
                    .to_string(),
            },
        );

        let width = 40usize;
        let mut terminal = Terminal::new(TestBackend::new(width as u16, 20))?;
        terminal.draw(|frame| model.render(frame))?;
        let rows = buffer_rows(&terminal, width);

        assert!(
            rows.iter().any(|r| r.contains("run_command")),
            "tool name missing: {rows:?}"
        );
        // glyph (2) + "run_command" (11) + space (1) = 14-column hanging indent.
        let indent = " ".repeat(14);
        assert!(
            rows.iter()
                .any(|r| r.starts_with(&indent) && !r.trim().is_empty()),
            "args did not hang-indent on continuation: {rows:?}"
        );
        Ok(())
    }

    #[test]
    fn dashboard_enter_resumes_selected_thread() {
        let mut model = UiModel::new();
        let thread_id = ThreadId::new();
        model.dashboard.items = vec![ThreadListItem {
            thread_id: thread_id.to_string(),
            rollout_path: "session.jsonl".to_string(),
            created_at: "2026-05-31T00:00:00Z".to_string(),
            updated_at: "2026-05-31T00:01:00Z".to_string(),
            last_user_message: Some("hello".to_string()),
            last_assistant_message: None,
        }];

        let effects = model.handle_key_event(key(KeyCode::Enter));

        assert_eq!(effects.len(), 1);
        assert!(matches!(
            effects[0].kind,
            UiEffectKind::ThreadResume { thread_id: got } if got == thread_id
        ));
    }

    #[test]
    fn vim_modes_and_send_command_start_turn() {
        let mut model = UiModel::new();
        let thread_id = ThreadId::new();
        model.current_thread_id = Some(thread_id);
        model.screen = Screen::Workspace;
        model.composer.set_text("hello".to_string());

        let _ = model.handle_key_event(key(KeyCode::Char(':')));
        for ch in "send".chars() {
            let _ = model.handle_key_event(key(KeyCode::Char(ch)));
        }
        let effects = model.handle_key_event(key(KeyCode::Enter));

        assert_eq!(effects.len(), 1);
        assert!(matches!(
            &effects[0].kind,
            UiEffectKind::TurnStart {
                thread_id: got,
                input,
            } if *got == thread_id && input == "hello"
        ));
        assert!(model.composer.is_empty());
        assert_eq!(model.composer.cursor(), 0);
    }

    #[test]
    fn ctrl_c_exits_across_modes() {
        for mode in [
            UiMode::Normal,
            UiMode::Insert,
            UiMode::Command,
            UiMode::Overlay,
        ] {
            let mut model = UiModel::new();
            model.screen = Screen::Workspace;
            model.mode = mode;
            if mode == UiMode::Overlay {
                model.question_picker = Some(QuestionPicker::new(
                    RequestId(1),
                    AskUserQuestionParams {
                        thread_id: "t".into(),
                        turn_id: "u".into(),
                        questions: Vec::new(),
                    },
                ));
            }
            let effects =
                model.handle_key_event(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
            assert_eq!(effects.len(), 1);
            assert!(matches!(effects[0].kind, UiEffectKind::Exit));
        }
    }

    #[test]
    fn ctrl_c_cancels_running_turn_across_modes() {
        for mode in [
            UiMode::Normal,
            UiMode::Insert,
            UiMode::Command,
            UiMode::Overlay,
        ] {
            let mut model = UiModel::new();
            let thread_id = ThreadId::new();
            model.current_thread_id = Some(thread_id);
            model.screen = Screen::Workspace;
            model.mode = mode;
            model.is_turn_running = true;
            model.current_turn_id = Some("turn-1".to_string());
            if mode == UiMode::Overlay {
                model.question_picker = Some(QuestionPicker::new(
                    RequestId(1),
                    AskUserQuestionParams {
                        thread_id: thread_id.to_string(),
                        turn_id: "turn-1".into(),
                        questions: Vec::new(),
                    },
                ));
            }

            let effects =
                model.handle_key_event(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));

            assert_eq!(effects.len(), 1);
            assert!(matches!(
                effects[0].kind,
                UiEffectKind::TurnCancel { thread_id: got } if got == thread_id
            ));
            assert!(model.is_turn_cancelling);
            assert_eq!(model.status_line, "Cancelling turn");
        }
    }

    #[test]
    fn esc_does_not_cancel_running_turn_in_normal_mode() {
        let mut model = UiModel::new();
        let thread_id = ThreadId::new();
        model.current_thread_id = Some(thread_id);
        model.screen = Screen::Workspace;
        model.mode = UiMode::Normal;
        model.is_turn_running = true;
        model.current_turn_id = Some("turn-1".to_string());

        let effects = model.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert!(
            effects.is_empty(),
            "interrupting is reserved for Ctrl-C; Esc must be a no-op"
        );
        assert!(!model.is_turn_cancelling);
    }

    #[test]
    fn esc_dismisses_overlay_instead_of_cancelling_turn() {
        let mut model = UiModel::new();
        let thread_id = ThreadId::new();
        model.current_thread_id = Some(thread_id);
        model.screen = Screen::Workspace;
        model.mode = UiMode::Overlay;
        model.is_turn_running = true;
        model.current_turn_id = Some("turn-1".to_string());
        model.question_picker = Some(QuestionPicker::new(
            RequestId(1),
            AskUserQuestionParams {
                thread_id: thread_id.to_string(),
                turn_id: "turn-1".into(),
                questions: vec![app_server_protocol::AskUserQuestion {
                    question: "Proceed?".to_string(),
                    header: "Choice".to_string(),
                    options: vec![app_server_protocol::AskUserQuestionOption {
                        label: "Yes".to_string(),
                        description: "go ahead".to_string(),
                        preview: None,
                    }],
                    multi_select: false,
                }],
            },
        ));

        let effects = model.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect.kind, UiEffectKind::TurnCancel { .. })),
            "overlay Esc must dismiss the picker, not cancel the turn"
        );
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect.kind, UiEffectKind::FailQuestion { .. })),
            "overlay Esc should decline the question"
        );
        assert!(!model.is_turn_cancelling);
        assert!(model.question_picker.is_none());
    }

    #[test]
    fn esc_in_insert_mode_returns_to_normal_even_while_turn_running() {
        let mut model = UiModel::new();
        let thread_id = ThreadId::new();
        model.current_thread_id = Some(thread_id);
        model.screen = Screen::Workspace;
        model.mode = UiMode::Insert;
        model.is_turn_running = true;

        let effects = model.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert!(
            !effects
                .iter()
                .any(|effect| matches!(effect.kind, UiEffectKind::TurnCancel { .. })),
            "insert-mode Esc must switch modes, not cancel the turn"
        );
        assert_eq!(model.mode, UiMode::Normal);
        assert!(!model.is_turn_cancelling);
    }

    #[test]
    fn cancel_command_emits_turn_cancel_effect() {
        let mut model = UiModel::new();
        let thread_id = ThreadId::new();
        model.current_thread_id = Some(thread_id);
        model.screen = Screen::Workspace;
        model.is_turn_running = true;

        let effects = model.execute_command("cancel");

        assert_eq!(effects.len(), 1);
        assert!(matches!(
            effects[0].kind,
            UiEffectKind::TurnCancel { thread_id: got } if got == thread_id
        ));
        assert!(model.is_turn_cancelling);
    }

    #[test]
    fn failed_cancel_restores_running_status_and_reports_error() {
        let mut model = UiModel::new();
        let thread_id = ThreadId::new();
        model.current_thread_id = Some(thread_id);
        model.screen = Screen::Workspace;
        model.is_turn_running = true;
        model.current_turn_id = Some("turn-1".to_string());

        let effects = model.request_turn_cancel();
        assert_eq!(effects.len(), 1);
        assert!(model.is_turn_cancelling);

        let _ = model.update(UiEvent::EffectFailed {
            effect_id: effects[0].effect_id,
            error: "server down".to_string(),
            viewport_height: 20,
        });

        assert!(model.is_turn_running);
        assert!(!model.is_turn_cancelling);
        assert_eq!(model.status_line, "Running turn turn-1");
        assert!(
            model
                .transcript_lines_uncached(80)
                .join("\n")
                .contains("could not cancel turn")
        );
    }

    #[test]
    fn turn_interrupted_closes_question_picker_overlay() {
        let mut model = UiModel::new();
        let thread_id = ThreadId::new();
        model.current_thread_id = Some(thread_id);
        model.screen = Screen::Workspace;
        model.mode = UiMode::Overlay;
        model.focus = FocusTarget::Overlay;
        model.is_turn_running = true;
        model.is_turn_cancelling = true;
        model.question_picker = Some(QuestionPicker::new(
            RequestId(1),
            AskUserQuestionParams {
                thread_id: thread_id.to_string(),
                turn_id: "turn-1".into(),
                questions: Vec::new(),
            },
        ));

        model.apply_protocol_event(event(
            "interrupted",
            EventMsg::TurnInterrupted(TurnInterruptedEvent {
                thread_id: thread_id.to_string(),
                turn_id: "turn-1".to_string(),
                reason: "interrupted".to_string(),
            }),
        ));

        assert!(model.question_picker.is_none());
        assert_eq!(model.mode, UiMode::Normal);
        assert_eq!(model.focus, FocusTarget::Transcript);
        assert!(!model.is_turn_running);
        assert!(!model.is_turn_cancelling);
    }

    fn plan_approval_params(thread_id: &str) -> RequestPlanApprovalParams {
        RequestPlanApprovalParams {
            thread_id: thread_id.to_string(),
            turn_id: "turn-1".to_string(),
            call_id: "call-1".to_string(),
            plan: "# The plan\n\n1. Refactor the module.".to_string(),
        }
    }

    #[test]
    fn active_plan_approval_request_opens_overlay_and_renders_plan()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut model = UiModel::new();
        let thread_id = ThreadId::new();
        model.current_thread_id = Some(thread_id);

        let effects = model.update(UiEvent::ServerRequest(ServerRequest::RequestPlanApproval {
            request_id: RequestId(50),
            params: plan_approval_params(&thread_id.to_string()),
        }));

        assert!(effects.is_empty());
        assert_eq!(model.screen, Screen::Workspace);
        assert_eq!(model.mode, UiMode::Overlay);
        assert!(model.plan_approval.is_some());

        let mut terminal = Terminal::new(TestBackend::new(80, 24))?;
        terminal.draw(|frame| model.render(frame))?;
        let rendered = rendered_buffer_text(&terminal);
        assert!(rendered.contains("Plan approval"), "{rendered}");
        assert!(rendered.contains("The plan"), "{rendered}");
        assert!(rendered.contains("Refactor the module."), "{rendered}");
        Ok(())
    }

    #[test]
    fn plan_approval_request_from_inactive_thread_is_failed() {
        let mut model = UiModel::new();
        model.current_thread_id = Some(ThreadId::new());
        let stale_thread = ThreadId::new();

        let effects = model.update(UiEvent::ServerRequest(ServerRequest::RequestPlanApproval {
            request_id: RequestId(51),
            params: plan_approval_params(&stale_thread.to_string()),
        }));

        assert_eq!(effects.len(), 1);
        assert!(model.plan_approval.is_none());
        assert!(matches!(
            &effects[0].kind,
            UiEffectKind::FailServerRequest { request_id, error }
                if *request_id == RequestId(51)
                    && error.message.contains("inactive thread")
        ));
    }

    #[test]
    fn plan_approval_request_while_picker_pending_is_failed() {
        let mut model = UiModel::new();
        let thread_id = ThreadId::new();
        model.current_thread_id = Some(thread_id);
        model.question_picker = Some(QuestionPicker::new(
            RequestId(1),
            AskUserQuestionParams {
                thread_id: thread_id.to_string(),
                turn_id: "turn-1".into(),
                questions: Vec::new(),
            },
        ));

        let effects = model.update(UiEvent::ServerRequest(ServerRequest::RequestPlanApproval {
            request_id: RequestId(52),
            params: plan_approval_params(&thread_id.to_string()),
        }));

        assert!(model.plan_approval.is_none());
        assert!(matches!(
            &effects[0].kind,
            UiEffectKind::FailServerRequest { request_id, error }
                if *request_id == RequestId(52)
                    && error.message.contains("already pending")
        ));
    }

    #[test]
    fn ask_user_question_request_while_overlay_pending_is_failed() {
        let mut model = UiModel::new();
        let thread_id = ThreadId::new();
        model.current_thread_id = Some(thread_id);
        model.question_picker = Some(QuestionPicker::new(
            RequestId(1),
            AskUserQuestionParams {
                thread_id: thread_id.to_string(),
                turn_id: "turn-1".into(),
                questions: Vec::new(),
            },
        ));

        let effects = model.update(UiEvent::ServerRequest(ServerRequest::AskUserQuestion {
            request_id: RequestId(2),
            params: AskUserQuestionParams {
                thread_id: thread_id.to_string(),
                turn_id: "turn-1".into(),
                questions: Vec::new(),
            },
        }));

        // The new request fails; the first picker stays untouched.
        assert!(matches!(
            &effects[0].kind,
            UiEffectKind::FailServerRequest { request_id, error }
                if *request_id == RequestId(2)
                    && error.message.contains("already pending")
        ));
        let Some(picker) = model.question_picker.as_ref() else {
            panic!("first picker should remain pending");
        };
        assert_eq!(picker.request_id, RequestId(1));
    }

    #[test]
    fn approving_plan_emits_respond_effect_and_closes_overlay() {
        let mut model = UiModel::new();
        let thread_id = ThreadId::new();
        model.current_thread_id = Some(thread_id);
        let _ = model.update(UiEvent::ServerRequest(ServerRequest::RequestPlanApproval {
            request_id: RequestId(53),
            params: plan_approval_params(&thread_id.to_string()),
        }));

        let effects = model.handle_key_event(key(KeyCode::Char('a')));

        assert!(model.plan_approval.is_none());
        assert_eq!(model.mode, UiMode::Normal);
        assert_eq!(model.focus, FocusTarget::Transcript);
        assert!(matches!(
            &effects[0].kind,
            UiEffectKind::RespondPlanApproval { request_id, response }
                if *request_id == RequestId(53)
                    && response.decision == PlanApprovalDecision::Approved
                    && response.feedback.is_none()
        ));
    }

    #[test]
    fn rejecting_plan_with_feedback_emits_respond_effect() {
        let mut model = UiModel::new();
        let thread_id = ThreadId::new();
        model.current_thread_id = Some(thread_id);
        let _ = model.update(UiEvent::ServerRequest(ServerRequest::RequestPlanApproval {
            request_id: RequestId(54),
            params: plan_approval_params(&thread_id.to_string()),
        }));

        let _ = model.handle_key_event(key(KeyCode::Char('r')));
        for ch in "no tests".chars() {
            let _ = model.handle_key_event(key(KeyCode::Char(ch)));
        }
        let effects = model.handle_key_event(key(KeyCode::Enter));

        assert!(model.plan_approval.is_none());
        assert!(matches!(
            &effects[0].kind,
            UiEffectKind::RespondPlanApproval { request_id, response }
                if *request_id == RequestId(54)
                    && response.decision == PlanApprovalDecision::Rejected
                    && response.feedback.as_deref() == Some("no tests")
        ));
    }

    #[test]
    fn turn_interrupted_closes_plan_approval_overlay() {
        let mut model = UiModel::new();
        let thread_id = ThreadId::new();
        model.current_thread_id = Some(thread_id);
        model.screen = Screen::Workspace;
        model.mode = UiMode::Overlay;
        model.focus = FocusTarget::Overlay;
        model.is_turn_running = true;
        model.plan_approval = Some(PlanApprovalOverlay::new(
            RequestId(55),
            plan_approval_params(&thread_id.to_string()),
        ));

        model.apply_protocol_event(event(
            "interrupted",
            EventMsg::TurnInterrupted(TurnInterruptedEvent {
                thread_id: thread_id.to_string(),
                turn_id: "turn-1".to_string(),
                reason: "interrupted".to_string(),
            }),
        ));

        assert!(model.plan_approval.is_none());
        assert_eq!(model.mode, UiMode::Normal);
        assert_eq!(model.focus, FocusTarget::Transcript);
    }

    fn select_model_with_items(count: usize) -> UiModel {
        let mut model = workspace_normal_model();
        for idx in 0..count {
            let id = model.next_item_id();
            model.push_history(TranscriptItem::info(id, format!("item {idx}")));
        }
        model
    }

    fn enter_select(model: &mut UiModel, t0: Instant) {
        let _ = model.handle_key_event_at(key(KeyCode::Esc), t0);
        let _ = model.handle_key_event_at(key(KeyCode::Esc), t0 + Duration::from_millis(100));
    }

    fn clipboard_content(effects: &[UiEffect]) -> Option<&str> {
        effects.iter().find_map(|effect| match &effect.kind {
            UiEffectKind::CopyToClipboard { content } => Some(content.as_str()),
            _ => None,
        })
    }

    #[test]
    fn double_esc_within_window_enters_transcript_select() {
        let mut model = select_model_with_items(3);
        enter_select(&mut model, Instant::now());

        assert_eq!(model.mode, UiMode::TranscriptSelect);
        assert!(!model.auto_scroll);
        assert_eq!(model.transcript_select.map(|state| state.selected), Some(2));
    }

    #[test]
    fn slow_double_esc_stays_normal() {
        let mut model = select_model_with_items(1);
        let t0 = Instant::now();
        let _ = model.handle_key_event_at(key(KeyCode::Esc), t0);
        let _ = model.handle_key_event_at(key(KeyCode::Esc), t0 + Duration::from_millis(600));

        assert_eq!(model.mode, UiMode::Normal);
        assert!(model.transcript_select.is_none());
    }

    #[test]
    fn esc_from_insert_then_esc_enters_select() {
        let mut model = workspace_insert_model();
        let id = model.next_item_id();
        model.push_history(TranscriptItem::info(id, "row"));

        let t0 = Instant::now();
        let _ = model.handle_key_event_at(key(KeyCode::Esc), t0);
        assert_eq!(model.mode, UiMode::Normal);
        let _ = model.handle_key_event_at(key(KeyCode::Esc), t0 + Duration::from_millis(100));

        assert_eq!(model.mode, UiMode::TranscriptSelect);
    }

    #[test]
    fn key_between_escs_breaks_chord() {
        let mut model = select_model_with_items(1);
        let t0 = Instant::now();
        let _ = model.handle_key_event_at(key(KeyCode::Esc), t0);
        let _ = model.handle_key_event_at(key(KeyCode::Char('j')), t0 + Duration::from_millis(50));
        let _ = model.handle_key_event_at(key(KeyCode::Esc), t0 + Duration::from_millis(100));

        assert_eq!(model.mode, UiMode::Normal);
    }

    #[test]
    fn double_esc_on_empty_transcript_stays_normal() {
        let mut model = workspace_normal_model();
        enter_select(&mut model, Instant::now());

        assert_eq!(model.mode, UiMode::Normal);
        assert!(model.transcript_select.is_none());
        assert_eq!(model.status_line, "Transcript is empty");
    }

    #[test]
    fn select_navigation_clamps_and_scrolls() {
        let mut model = select_model_with_items(8);
        model.viewport_height = 4;
        model.transcript_inner_width = 40;
        let t0 = Instant::now();
        enter_select(&mut model, t0);

        // Starts on the last item, scrolled so it is visible.
        assert_eq!(model.transcript_select.map(|state| state.selected), Some(7));
        assert!(model.scroll > 0);

        let t = t0 + Duration::from_millis(200);
        let _ = model.handle_key_event_at(key(KeyCode::Char('j')), t);
        assert_eq!(
            model.transcript_select.map(|state| state.selected),
            Some(7),
            "j clamps at the last item"
        );

        let _ = model.handle_key_event_at(key(KeyCode::Char('g')), t);
        let _ = model.handle_key_event_at(key(KeyCode::Char('g')), t);
        assert_eq!(
            model.transcript_select.map(|state| state.selected),
            Some(0),
            "gg jumps to the first item"
        );
        assert_eq!(model.scroll, 0, "jumping to the first item scrolls to top");

        let _ = model.handle_key_event_at(key(KeyCode::Char('k')), t);
        assert_eq!(
            model.transcript_select.map(|state| state.selected),
            Some(0),
            "k clamps at the first item"
        );
    }

    #[test]
    fn select_esc_and_q_exit_without_app_exit() {
        for code in [KeyCode::Esc, KeyCode::Char('q')] {
            let mut model = select_model_with_items(2);
            let t0 = Instant::now();
            enter_select(&mut model, t0);
            assert_eq!(model.mode, UiMode::TranscriptSelect);

            let effects = model.handle_key_event_at(key(code), t0 + Duration::from_secs(2));
            assert!(effects.is_empty(), "{code:?} must not exit the app");
            assert_eq!(model.mode, UiMode::Normal);
            assert!(model.transcript_select.is_none());
        }
    }

    #[test]
    fn y_copies_tool_result_then_second_y_copies_args() {
        let mut app = App::new();
        start_turn(&mut app);
        start_tool_call(&mut app, "1", "call-1", "run_command", "{\"cmd\":\"ls\"}");
        complete_tool_call(&mut app, "2", "call-1", true, None);

        let t0 = Instant::now();
        enter_select(&mut app.model, t0);
        assert_eq!(app.model.mode, UiMode::TranscriptSelect);

        let effects = app
            .model
            .handle_key_event_at(key(KeyCode::Char('y')), t0 + Duration::from_millis(200));
        assert_eq!(clipboard_content(&effects), Some("BIG CONTENT"));

        let effects = app
            .model
            .handle_key_event_at(key(KeyCode::Char('y')), t0 + Duration::from_millis(300));
        assert_eq!(clipboard_content(&effects), Some("{\"cmd\":\"ls\"}"));

        // After the chord window the next y copies the result again.
        let effects = app
            .model
            .handle_key_event_at(key(KeyCode::Char('y')), t0 + Duration::from_millis(1200));
        assert_eq!(clipboard_content(&effects), Some("BIG CONTENT"));
    }

    #[test]
    fn y_on_running_tool_falls_back_to_args() {
        let mut app = App::new();
        start_turn(&mut app);
        start_tool_call(
            &mut app,
            "1",
            "call-1",
            "run_command",
            "{\"cmd\":\"sleep\"}",
        );

        let t0 = Instant::now();
        enter_select(&mut app.model, t0);
        let effects = app
            .model
            .handle_key_event_at(key(KeyCode::Char('y')), t0 + Duration::from_millis(200));
        assert_eq!(clipboard_content(&effects), Some("{\"cmd\":\"sleep\"}"));
    }

    #[test]
    fn y_copies_user_message_and_assistant_raw_markdown() {
        let mut app = App::new();
        start_turn(&mut app);
        app.handle_session_event(
            event(
                "1",
                EventMsg::UserMessage {
                    text: String::from("hello there"),
                },
            ),
            20,
        );
        let markdown = "answer:\n\n```rust\nlet x = 1;\n```";
        complete_agent_message(&mut app, "2", "assistant-1", markdown);

        let t0 = Instant::now();
        enter_select(&mut app.model, t0);
        // Last item is the assistant message.
        let effects = app
            .model
            .handle_key_event_at(key(KeyCode::Char('y')), t0 + Duration::from_millis(200));
        assert_eq!(clipboard_content(&effects), Some(markdown));

        let t = t0 + Duration::from_secs(2);
        let _ = app.model.handle_key_event_at(key(KeyCode::Char('k')), t);
        let effects = app
            .model
            .handle_key_event_at(key(KeyCode::Char('y')), t + Duration::from_millis(100));
        assert_eq!(clipboard_content(&effects), Some("hello there"));
    }

    #[test]
    fn selected_item_lines_get_background_highlight() {
        let mut model = select_model_with_items(2);
        model.transcript_inner_width = 40;
        enter_select(&mut model, Instant::now());

        let lines = model.visible_transcript_lines(40, 10);
        // Layout: item0 row, separator, item1 row (selected).
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].style.bg, None);
        assert_eq!(lines[2].style.bg, Some(Color::DarkGray));
    }

    #[test]
    fn turn_interrupted_exits_select_mode() {
        let mut model = select_model_with_items(2);
        let thread_id = ThreadId::new();
        model.current_thread_id = Some(thread_id);
        enter_select(&mut model, Instant::now());
        assert_eq!(model.mode, UiMode::TranscriptSelect);

        model.apply_protocol_event(event(
            "interrupted",
            EventMsg::TurnInterrupted(TurnInterruptedEvent {
                thread_id: thread_id.to_string(),
                turn_id: "turn-1".to_string(),
                reason: "interrupted".to_string(),
            }),
        ));

        assert_eq!(model.mode, UiMode::Normal);
        assert!(model.transcript_select.is_none());
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

    #[test]
    fn clipboard_payload_truncates_on_char_boundary() {
        let long = "é".repeat(MAX_CLIPBOARD_BYTES);
        let (clipped, truncated) = clip_for_clipboard(long);
        assert!(truncated);
        assert!(clipped.len() <= MAX_CLIPBOARD_BYTES);
        assert!(clipped.chars().all(|c| c == 'é'));

        let (kept, truncated) = clip_for_clipboard(String::from("short"));
        assert!(!truncated);
        assert_eq!(kept, "short");
    }

    trait JoinLines {
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

    // --- subagent preview stack ---

    fn preview_response(
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

    fn open_preview(model: &mut UiModel, thread_id: ThreadId, messages: Vec<EventMsg>) {
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

    fn unwatch_targets(effects: &[UiEffect]) -> Vec<ThreadId> {
        effects
            .iter()
            .filter_map(|effect| match effect.kind {
                UiEffectKind::ThreadUnwatch { thread_id } => Some(thread_id),
                _ => None,
            })
            .collect()
    }

    fn child_event(id: &str, msg: EventMsg) -> Event {
        Event {
            id: id.to_owned(),
            msg,
        }
    }

    #[test]
    fn preview_effect_result_pushes_view_and_replays_initial_messages()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut model = workspace_normal_model();
        let child = ThreadId::new();
        open_preview(
            &mut model,
            child,
            vec![
                EventMsg::UserMessage {
                    text: String::from("inspect the rollout"),
                },
                EventMsg::TurnStarted(TurnStartedEvent {
                    thread_id: child.to_string(),
                    turn_id: String::from("0"),
                }),
            ],
        );

        assert_eq!(model.preview_stack.len(), 1);
        let view = model.preview_stack.last().ok_or("view")?;
        assert_eq!(view.thread_id, child);
        assert_eq!(view.item_count(), 1, "the user message becomes one item");
        // The response status wins over replayed turn-lifecycle events.
        assert_eq!(view.status, AgentStatus::Running);

        let mut terminal = Terminal::new(TestBackend::new(60, 12))?;
        terminal.draw(|frame| model.render(frame))?;
        let rendered = rendered_buffer_text(&terminal);
        assert!(rendered.contains("subagent worker"), "{rendered}");
        assert!(rendered.contains("running"), "{rendered}");
        assert!(rendered.contains("inspect the rollout"), "{rendered}");
        assert!(rendered.contains("q back"), "{rendered}");
        Ok(())
    }

    #[test]
    fn preview_renders_status_line_for_in_preview_gd_failures()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut model = workspace_normal_model();
        let child = ThreadId::new();
        open_preview(
            &mut model,
            child,
            vec![EventMsg::UserMessage {
                text: String::from("not a tool row"),
            }],
        );

        // `gd` on a non-subagent row inside the preview: the feedback must be
        // visible without leaving the preview. `gd` lives in the select
        // sub-mode, reached with `Esc Esc`.
        let t0 = Instant::now();
        let _ = model.handle_key_event_at(key(KeyCode::Esc), t0);
        let _ = model.handle_key_event_at(key(KeyCode::Esc), t0 + Duration::from_millis(50));
        let _ = model.handle_key_event_at(key(KeyCode::Char('g')), t0 + Duration::from_millis(100));
        let effects =
            model.handle_key_event_at(key(KeyCode::Char('d')), t0 + Duration::from_millis(150));
        assert!(effects.is_empty());

        let mut terminal = Terminal::new(TestBackend::new(80, 12))?;
        terminal.draw(|frame| model.render(frame))?;
        let rendered = rendered_buffer_text(&terminal);
        assert!(rendered.contains("Not a subagent row"), "{rendered}");
        Ok(())
    }

    #[test]
    fn child_events_route_to_every_matching_preview_not_main_transcript() {
        let mut model = workspace_normal_model();
        let parent = ThreadId::new();
        let child = ThreadId::new();
        model.current_thread_id = Some(parent);
        let main_items_before = model.transcript_items.len();

        // The same thread pushed twice: both views must receive the event.
        open_preview(&mut model, child, Vec::new());
        open_preview(&mut model, child, Vec::new());

        let effects = model.update(UiEvent::Protocol {
            source_thread_id: Some(child),
            event: Box::new(child_event(
                "c1",
                EventMsg::UserMessage {
                    text: String::from("child input"),
                },
            )),
            viewport_height: 20,
        });
        assert!(effects.is_empty());
        for view in &model.preview_stack {
            assert_eq!(view.item_count(), 1, "both duplicate views update");
        }
        assert_eq!(
            model.transcript_items.len(),
            main_items_before,
            "child events must not leak into the main transcript"
        );

        // Current-thread events still reach the main transcript.
        let _ = model.update(UiEvent::Protocol {
            source_thread_id: Some(parent),
            event: Box::new(child_event(
                "p1",
                EventMsg::UserMessage {
                    text: String::from("parent input"),
                },
            )),
            viewport_height: 20,
        });
        assert_eq!(model.transcript_items.len(), main_items_before + 1);
    }

    #[test]
    fn nested_parent_completion_patches_child_view_status() {
        let mut model = workspace_normal_model();
        let parent = ThreadId::new();
        let view_a = ThreadId::new();
        let view_b = ThreadId::new();
        model.current_thread_id = Some(parent);
        open_preview(&mut model, view_a, Vec::new());
        open_preview(&mut model, view_b, Vec::new());

        // B's completion arrives on A's channel (A spawned B): view B's
        // status must be patched even though source routing feeds view A.
        let _ = model.update(UiEvent::Protocol {
            source_thread_id: Some(view_a),
            event: Box::new(child_event(
                "done",
                EventMsg::CollabAgentCompleted(smooth_protocol::CollabAgentCompletedEvent {
                    parent_thread_id: view_a,
                    child_thread_id: view_b,
                    agent_path: smooth_protocol::AgentPath::try_from("/root/a/b")
                        .unwrap_or_else(|_| panic!("agent path")),
                    agent_nickname: Some(String::from("b")),
                    status: AgentStatus::Completed(Some(String::from("done"))),
                    last_assistant_message: Some(String::from("done")),
                }),
            )),
            viewport_height: 20,
        });

        let b = model
            .preview_stack
            .iter()
            .find(|view| view.thread_id == view_b)
            .unwrap_or_else(|| panic!("view B"));
        assert_eq!(b.status, AgentStatus::Completed(Some(String::from("done"))));
        assert!(!b.is_live);
        let a = model
            .preview_stack
            .iter()
            .find(|view| view.thread_id == view_a)
            .unwrap_or_else(|| panic!("view A"));
        assert_eq!(a.item_count(), 1, "view A renders the completion info row");
    }

    #[test]
    fn preview_pop_emits_one_unwatch_per_view_and_preserves_parent_state() {
        let mut model = select_model_with_items(4);
        let t0 = Instant::now();
        enter_select(&mut model, t0);
        if let Some(state) = model.transcript_select.as_mut() {
            state.selected = 2;
        }
        let parent_scroll = model.scroll;

        let child = ThreadId::new();
        open_preview(&mut model, child, Vec::new());
        open_preview(&mut model, child, Vec::new());

        let t = t0 + Duration::from_millis(300);
        let effects = model.handle_key_event_at(key(KeyCode::Char('q')), t);
        assert_eq!(
            unwatch_targets(&effects),
            vec![child],
            "first pop unwatches"
        );
        assert_eq!(model.preview_stack.len(), 1);

        let effects = model.handle_key_event_at(key(KeyCode::Char('q')), t);
        assert_eq!(
            unwatch_targets(&effects),
            vec![child],
            "second pop unwatches again — the server refcount drains to zero"
        );
        assert!(model.preview_stack.is_empty());

        assert_eq!(model.mode, UiMode::TranscriptSelect);
        assert_eq!(model.transcript_select.map(|state| state.selected), Some(2));
        assert_eq!(model.scroll, parent_scroll);
    }

    #[test]
    fn nested_gd_pushes_second_view_and_duplicates_are_dropped() {
        let mut model = workspace_normal_model();
        let child = ThreadId::new();
        let grandchild = ThreadId::new();
        let started = EventMsg::ToolCallStarted(ToolCallStartedEvent {
            thread_id: child.to_string(),
            turn_id: String::from("0"),
            call_id: String::from("spawn-1"),
            tool_name: String::from("spawn_agent"),
            args_preview: String::from("{\"prompt\":\"dig deeper\"}"),
        });
        let completed = EventMsg::ToolCallCompleted(ToolCallCompletedEvent {
            thread_id: child.to_string(),
            turn_id: String::from("0"),
            call_id: String::from("spawn-1"),
            success: true,
            output_preview: Some(String::from("{\"status\":\"completed\"}")),
            error: None,
            result_kind: ToolCallResultKind::Final,
            related_thread_id: Some(grandchild),
            file_change: None,
            file_changes: Vec::new(),
            todos: Vec::new(),
        });
        open_preview(&mut model, child, vec![started.clone(), completed.clone()]);
        let items_after_replay = model
            .preview_stack
            .last()
            .map(SubagentPreviewView::item_count)
            .unwrap_or_default();

        // The subscribe-then-snapshot overlap can replay both events again.
        for (id, msg) in [("dup-1", started), ("dup-2", completed)] {
            let _ = model.update(UiEvent::Protocol {
                source_thread_id: Some(child),
                event: Box::new(child_event(id, msg)),
                viewport_height: 20,
            });
        }
        let view = model
            .preview_stack
            .last()
            .unwrap_or_else(|| panic!("preview view"));
        assert_eq!(
            view.item_count(),
            items_after_replay,
            "duplicate started/completed events must not add rows"
        );

        // `gd` on the spawn row nests a second preview. `gd` lives in the
        // select sub-mode, reached with `Esc Esc`.
        let t0 = Instant::now();
        let _ = model.handle_key_event_at(key(KeyCode::Esc), t0);
        let _ = model.handle_key_event_at(key(KeyCode::Esc), t0 + Duration::from_millis(50));
        let _ = model.handle_key_event_at(key(KeyCode::Char('g')), t0 + Duration::from_millis(100));
        let effects =
            model.handle_key_event_at(key(KeyCode::Char('d')), t0 + Duration::from_millis(150));
        let targets: Vec<ThreadId> = effects
            .iter()
            .filter_map(|effect| match effect.kind {
                UiEffectKind::ThreadPreview { thread_id } => Some(thread_id),
                _ => None,
            })
            .collect();
        assert_eq!(targets, vec![grandchild]);
        open_preview(&mut model, grandchild, Vec::new());
        assert_eq!(model.preview_stack.len(), 2);
    }

    #[test]
    fn preview_opens_in_scroll_mode_and_jk_keeps_selection() {
        let mut model = workspace_normal_model();
        let child = ThreadId::new();
        open_preview(
            &mut model,
            child,
            vec![
                EventMsg::UserMessage {
                    text: String::from("first"),
                },
                EventMsg::AgentMessage {
                    text: String::from("second"),
                },
                EventMsg::UserMessage {
                    text: String::from("third"),
                },
            ],
        );
        let view = model.preview_stack.last().expect("view");
        assert!(!view.select_mode, "preview opens in the scroll sub-mode");
        let selected_before = view.selected;

        let t0 = Instant::now();
        let _ = model.handle_key_event_at(key(KeyCode::Char('k')), t0);
        let _ = model.handle_key_event_at(key(KeyCode::Char('j')), t0 + Duration::from_millis(10));
        let view = model.preview_stack.last().expect("view");
        assert!(!view.select_mode);
        assert_eq!(
            view.selected, selected_before,
            "scroll-mode j/k must not move the selection cursor"
        );
    }

    #[test]
    fn preview_double_esc_enters_select_mode_and_jk_moves_selection() {
        let mut model = workspace_normal_model();
        let child = ThreadId::new();
        open_preview(
            &mut model,
            child,
            vec![
                EventMsg::UserMessage {
                    text: String::from("first"),
                },
                EventMsg::AgentMessage {
                    text: String::from("second"),
                },
                EventMsg::UserMessage {
                    text: String::from("third"),
                },
            ],
        );

        let t0 = Instant::now();
        let _ = model.handle_key_event_at(key(KeyCode::Esc), t0);
        let _ = model.handle_key_event_at(key(KeyCode::Esc), t0 + Duration::from_millis(50));
        let view = model.preview_stack.last().expect("view");
        assert!(view.select_mode, "double-Esc enters the select sub-mode");
        let selected_after_enter = view.selected;

        let _ = model.handle_key_event_at(key(KeyCode::Char('j')), t0 + Duration::from_millis(100));
        let view = model.preview_stack.last().expect("view");
        assert!(view.select_mode);
        assert_eq!(
            view.selected,
            selected_after_enter + 1,
            "select-mode j moves the selection down"
        );
    }

    #[test]
    fn preview_gd_in_scroll_mode_does_not_open_nested() {
        let mut model = workspace_normal_model();
        let child = ThreadId::new();
        let grandchild = ThreadId::new();
        let started = EventMsg::ToolCallStarted(ToolCallStartedEvent {
            thread_id: child.to_string(),
            turn_id: String::from("0"),
            call_id: String::from("spawn-1"),
            tool_name: String::from("spawn_agent"),
            args_preview: String::from("{\"prompt\":\"dig\"}"),
        });
        let completed = EventMsg::ToolCallCompleted(ToolCallCompletedEvent {
            thread_id: child.to_string(),
            turn_id: String::from("0"),
            call_id: String::from("spawn-1"),
            success: true,
            output_preview: Some(String::from("{\"status\":\"completed\"}")),
            error: None,
            result_kind: ToolCallResultKind::Final,
            related_thread_id: Some(grandchild),
            file_change: None,
            file_changes: Vec::new(),
            todos: Vec::new(),
        });
        open_preview(&mut model, child, vec![started, completed]);

        // In scroll mode `gd` must not open the nested subagent (it lives in
        // the select sub-mode); the preview stack is unchanged.
        let t0 = Instant::now();
        let _ = model.handle_key_event_at(key(KeyCode::Char('g')), t0);
        let effects =
            model.handle_key_event_at(key(KeyCode::Char('d')), t0 + Duration::from_millis(50));
        assert!(
            effects
                .iter()
                .all(|effect| !matches!(effect.kind, UiEffectKind::ThreadPreview { .. })),
            "gd in scroll mode must not emit a ThreadPreview effect"
        );
        assert_eq!(model.preview_stack.len(), 1);
    }

    #[test]
    fn preview_esc_exits_select_to_scroll_while_q_pops() {
        let mut model = workspace_normal_model();
        let child = ThreadId::new();
        open_preview(
            &mut model,
            child,
            vec![EventMsg::UserMessage {
                text: String::from("hi"),
            }],
        );

        let t0 = Instant::now();
        // Enter select, then Esc back to scroll — the preview stays open.
        let _ = model.handle_key_event_at(key(KeyCode::Esc), t0);
        let _ = model.handle_key_event_at(key(KeyCode::Esc), t0 + Duration::from_millis(50));
        assert!(model.preview_stack.last().expect("view").select_mode);
        let _ = model.handle_key_event_at(key(KeyCode::Esc), t0 + Duration::from_millis(100));
        let view = model.preview_stack.last().expect("view");
        assert!(!view.select_mode, "Esc in select returns to scroll");
        assert_eq!(
            model.preview_stack.len(),
            1,
            "Esc in select must not pop the preview"
        );

        // `q` in scroll mode pops the preview with exactly one unwatch.
        let effects =
            model.handle_key_event_at(key(KeyCode::Char('q')), t0 + Duration::from_millis(150));
        assert!(model.preview_stack.is_empty());
        assert_eq!(unwatch_targets(&effects), vec![child]);
    }

    #[test]
    fn server_request_for_previewed_child_is_accepted_and_clears_stack() {
        let mut model = workspace_normal_model();
        let parent = ThreadId::new();
        let child = ThreadId::new();
        model.current_thread_id = Some(parent);
        open_preview(&mut model, child, Vec::new());

        let request = ServerRequest::AskUserQuestion {
            request_id: RequestId(7),
            params: AskUserQuestionParams {
                thread_id: child.to_string(),
                turn_id: String::from("0"),
                questions: vec![AskUserQuestion {
                    question: String::from("Proceed?"),
                    header: String::from("Confirm"),
                    multi_select: false,
                    options: vec![
                        AskUserQuestionOption {
                            label: String::from("Yes"),
                            description: String::from("go"),
                            preview: None,
                        },
                        AskUserQuestionOption {
                            label: String::from("No"),
                            description: String::from("stop"),
                            preview: None,
                        },
                    ],
                }],
            },
        };
        let effects = model.update(UiEvent::ServerRequest(request));
        assert!(model.question_picker.is_some(), "the picker is shown");
        assert!(model.preview_stack.is_empty(), "the stack is closed first");
        assert_eq!(unwatch_targets(&effects), vec![child]);

        // A request for an unrelated thread is still rejected.
        let unrelated = ServerRequest::AskUserQuestion {
            request_id: RequestId(8),
            params: AskUserQuestionParams {
                thread_id: ThreadId::new().to_string(),
                turn_id: String::from("0"),
                questions: Vec::new(),
            },
        };
        model.question_picker = None;
        let effects = model.update(UiEvent::ServerRequest(unrelated));
        assert!(model.question_picker.is_none());
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect.kind, UiEffectKind::FailServerRequest { .. }))
        );
    }

    #[test]
    fn preview_mid_stream_completion_uses_full_text_not_tail() {
        let mut model = workspace_normal_model();
        let child = ThreadId::new();
        open_preview(&mut model, child, Vec::new());

        // The preview joined mid-stream: only the tail delta arrives.
        let _ = model.update(UiEvent::Protocol {
            source_thread_id: Some(child),
            event: Box::new(child_event(
                "d1",
                EventMsg::AgentMessageDelta(AgentMessageDeltaEvent {
                    thread_id: child.to_string(),
                    turn_id: String::from("0"),
                    item_id: String::from("m1"),
                    delta: String::from("tail of the message"),
                }),
            )),
            viewport_height: 20,
        });
        let _ = model.update(UiEvent::Protocol {
            source_thread_id: Some(child),
            event: Box::new(child_event(
                "c1",
                EventMsg::AgentMessageCompleted(AgentMessageCompletedEvent {
                    thread_id: child.to_string(),
                    turn_id: String::from("0"),
                    item_id: String::from("m1"),
                    text: String::from("the full message including the tail of the message"),
                }),
            )),
            viewport_height: 20,
        });

        let view = model
            .preview_stack
            .last()
            .unwrap_or_else(|| panic!("preview view"));
        assert_eq!(view.item_count(), 1, "one assistant item, not tail + full");
        let raw = view.items()[0]
            .copy_text()
            .unwrap_or_else(|| panic!("assistant raw text"));
        assert_eq!(
            raw, "the full message including the tail of the message",
            "the completed full text wins over the streamed tail"
        );
    }

    #[test]
    fn mismatched_preview_response_pushes_no_view_and_unwatches() {
        let mut model = workspace_normal_model();
        let requested = ThreadId::new();
        let other = ThreadId::new();

        let effect = model.effect(
            EffectContext::ThreadPreview {
                thread_id: requested,
            },
            UiEffectKind::ThreadPreview {
                thread_id: requested,
            },
        );
        let effects = model.update(UiEvent::EffectCompleted {
            effect_id: effect.effect_id,
            result: UiEffectResult::ThreadPreview(Box::new(preview_response(other, Vec::new()))),
            viewport_height: 20,
        });

        assert!(model.preview_stack.is_empty());
        assert_eq!(unwatch_targets(&effects), vec![requested]);
        assert!(model.status_line.contains("Could not open subagent"));
    }

    #[test]
    fn preview_effect_failure_defensively_unwatches() {
        let mut model = workspace_normal_model();
        let child = ThreadId::new();
        let effect = model.effect(
            EffectContext::ThreadPreview { thread_id: child },
            UiEffectKind::ThreadPreview { thread_id: child },
        );
        let effects = model.update(UiEvent::EffectFailed {
            effect_id: effect.effect_id,
            error: String::from("rollout missing"),
            viewport_height: 20,
        });
        assert!(model.preview_stack.is_empty());
        assert_eq!(unwatch_targets(&effects), vec![child]);
    }

    #[test]
    fn thread_switch_clears_preview_stack_with_unwatches() {
        let mut model = workspace_normal_model();
        let child = ThreadId::new();
        open_preview(&mut model, child, Vec::new());

        let effect = model.effect(EffectContext::ThreadStart, UiEffectKind::ThreadStart);
        let effects = model.update(UiEvent::EffectCompleted {
            effect_id: effect.effect_id,
            result: UiEffectResult::ThreadStart(ThreadStartResponse {
                thread_id: ThreadId::new().to_string(),
                rollout_path: String::from("rollout.jsonl"),
            }),
            viewport_height: 20,
        });
        assert!(model.preview_stack.is_empty());
        assert_eq!(unwatch_targets(&effects), vec![child]);
    }
}
