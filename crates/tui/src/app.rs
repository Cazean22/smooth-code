use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use app_server_protocol::{
    AskUserQuestionResponse, JsonRpcError, RequestId, RequestPlanApprovalResponse, ServerRequest,
    SetPlanModeResponse, ThreadListItem, ThreadListResponse, ThreadPreviewResponse,
    ThreadResumeResponse, ThreadStartResponse, TurnCancelResponse, TurnStartResponse,
};
use cazean_protocol::{
    AgentStatus, ErrorInfo, Event, EventMsg, FileChangeOutput, ThreadId, TodoItem,
    ToolCallResultKind,
};
use crossterm::event::{Event as CrosstermEvent, KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::Paragraph,
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

mod actions;
mod effects;
mod input;
mod preview;
mod protocol;
mod render;
mod scroll;
#[cfg(test)]
mod test_support;

#[derive(Debug)]
pub(crate) enum AppRunControl {
    Continue,
    Exit,
    /// Suspend the TUI and open this file in the user's `$EDITOR`; the run loop
    /// (which owns the terminal) handles suspend/resume.
    OpenEditor(PathBuf),
}

/// Window within which a second Esc press enters transcript-select mode.
const DOUBLE_ESC_WINDOW: Duration = Duration::from_millis(500);
/// Window within which a second `y` press upgrades a tool-result copy to args.
const COPY_CHORD_WINDOW: Duration = Duration::from_millis(500);
/// Window within which a `g` prefix completes as `gg` (top).
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
                UiEffectKind::OpenEditor { path } => {
                    return Ok(AppRunControl::OpenEditor(path));
                }
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

    /// After the external editor closes, refresh the approval overlay so it
    /// shows whatever the user saved.
    pub(crate) fn reload_plan_after_edit(&mut self) {
        self.model.reload_plan_after_edit();
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
    OpenEditor {
        path: PathBuf,
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
    OpenEditor,
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

/// State for transcript-select mode: which row/optional batch entry is selected
/// and, after a `y` on a tool row, the pending chance to upgrade the copy to the
/// selected tool args.
#[derive(Debug, Clone, Copy)]
struct TranscriptSelectState {
    selected: usize,
    selected_tool_entry: Option<usize>,
    pending_args: Option<(usize, Option<usize>, Instant)>,
    /// Armed by `g`: a second `g` within the chord window jumps to the top (`gg`).
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
    /// Skill roots searched when the composer holds a leading `/token`, in
    /// ascending precedence: the user-global dir (`~/.cazean/skills`) then the
    /// project dir (`<cwd>/.cazean/skills`), so a project skill overrides a
    /// same-named user-global one. Project-only in tests.
    skill_roots: Vec<std::path::PathBuf>,
    /// Workspace root (process cwd at startup), used to locate the plan file for
    /// `Ctrl+G` editing. The in-process core resolves the same path from its
    /// own creation-time cwd, so the two always agree.
    workspace_root: PathBuf,
    effect_counter: u64,
    effect_contexts: HashMap<EffectId, EffectContext>,
    screen: Screen,
    mode: UiMode,
    focus: FocusTarget,
    dashboard: DashboardState,
    transcript_select: Option<TranscriptSelectState>,
    /// Stacked full-screen subagent previews opened from transcript-select mode;
    /// the top view receives keys and is rendered, nesting pushes deeper.
    preview_stack: Vec<SubagentPreviewView>,
    /// Subagent previews parked with Ctrl-O, newest last. The intact views are
    /// kept here (still subscribed and still fed live events) so Ctrl-I restores
    /// them instantly with their in-flight stream — no server round-trip, no
    /// snapshot rebuild. The server watcher is released only when a view is truly
    /// discarded (stack cleared, or forward history invalidated by a new open).
    preview_forward_stack: Vec<SubagentPreviewView>,
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
            skill_roots: tools::skill_roots(
                cazean_config::user_skills_dir(),
                &std::env::current_dir().unwrap_or_default(),
            ),
            workspace_root: std::env::current_dir().unwrap_or_default(),
            effect_counter: 0,
            effect_contexts: HashMap::new(),
            screen: Screen::Dashboard,
            mode: UiMode::Normal,
            focus: FocusTarget::Dashboard,
            dashboard: DashboardState::default(),
            transcript_select: None,
            preview_stack: Vec::new(),
            preview_forward_stack: Vec::new(),
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
                // stack [A, B], B's completion arrives with source A). Parked
                // (Ctrl-O'd) views stay subscribed, so patch them too.
                if let EventMsg::CollabAgentCompleted(completed) = &event.msg {
                    for view in self
                        .preview_stack
                        .iter_mut()
                        .chain(self.preview_forward_stack.iter_mut())
                    {
                        if view.thread_id == completed.child_thread_id {
                            view.complete_from_parent(completed.status.clone());
                        }
                    }
                }
                // Events from previewed threads feed their stack views and
                // never touch the main transcript. Parked views are still
                // subscribed, so they stay in sync while backgrounded and
                // re-entry (Ctrl-I) shows everything that arrived meanwhile.
                if let Some(source) = source_thread_id
                    && self
                        .preview_stack
                        .iter()
                        .chain(self.preview_forward_stack.iter())
                        .any(|view| view.thread_id == source)
                {
                    // Previews render full-screen, so wrap at terminal width.
                    let width = self.terminal_width.max(1);
                    for view in self
                        .preview_stack
                        .iter_mut()
                        .chain(self.preview_forward_stack.iter_mut())
                    {
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

    /// Re-read the plan file into the active approval overlay after the user
    /// edits it in `$EDITOR`. No-op if there is no active overlay, the thread id
    /// didn't parse, or the file can't be read.
    fn reload_plan_after_edit(&mut self) {
        let Some(thread_id) = self
            .plan_approval
            .as_ref()
            .filter(|overlay| overlay.is_active())
            .and_then(PlanApprovalOverlay::thread_id)
        else {
            return;
        };
        let path = tools::plan_file_path(&self.workspace_root, thread_id);
        if let Ok(plan) = std::fs::read_to_string(&path)
            && let Some(overlay) = self.plan_approval.as_mut()
        {
            overlay.set_plan(plan);
        }
    }

    fn next_item_id(&mut self) -> TranscriptItemId {
        let id = self.next_transcript_item_id;
        self.next_transcript_item_id = self.next_transcript_item_id.saturating_add(1);
        id
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
