#![deny(clippy::unwrap_used, clippy::expect_used)]

mod app;
mod app_server_client;
mod app_server_session;
mod composer;
mod config_state;
mod diff_render;
mod error;
mod highlight;
mod history_cell;
mod markdown;
mod markdown_render;
mod markdown_stream;
mod plan_approval;
mod project_instructions;
mod question_picker;
mod skill_popup;
mod streaming;
mod subagent_preview;
mod wrap;

use std::io::{IsTerminal, Stdout};

use crate::{app::App, app_server_client::AppServerClient, app_server_session::AppServerSession};
use app_server::in_process::InProcessServerEvent;
use crossterm::{
    event::{
        DisableBracketedPaste, EnableBracketedPaste, KeyboardEnhancementFlags,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
    style::Print,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures_util::StreamExt;
use ratatui::{Terminal, prelude::CrosstermBackend};

pub use error::{TuiError, TuiResult};

pub type AppTerminal = Terminal<CrosstermBackend<Stdout>>;

#[tracing::instrument(name = "tui.run", skip_all)]
pub async fn run(config: std::sync::Arc<cazean_config::Config>) -> TuiResult<()> {
    // Make appearance settings available to the render layer, and reject an
    // unknown highlight theme before entering raw mode / the alternate screen.
    config_state::install(std::sync::Arc::clone(&config));
    validate_tui_config(&config)?;
    let mut terminal = init()?.ok_or(TuiError::TtyRequired)?;
    let mut app_server = AppServerSession::new(AppServerClient::start(512, config).await?);
    let mut app = App::new();
    let mut event_stream = crossterm::event::EventStream::new();
    // Raw mode delivers interactive Ctrl+C as a key event, so these streams
    // only fire for external kills (`kill <pid>`, terminal teardown, session
    // managers) — exactly the cases that previously left orphaned threads and
    // tool subprocesses behind.
    let mut signals = TerminateSignals::new()?;
    terminal.draw(|frame| app.render(frame))?;
    let viewport_height = app.viewport_height_for(&terminal)?;
    if matches!(
        app.startup(&mut app_server, viewport_height).await?,
        crate::app::AppRunControl::Exit
    ) {
        shutdown_app_server(&mut app_server).await;
        return restore(Some(&mut terminal));
    }
    terminal.draw(|frame| app.render(frame))?;

    loop {
        tokio::select! {
            event = app_server.next_event() => {
                match event {
                    Some(InProcessServerEvent::SessionEvent { thread_id, event }) => {
                        let viewport_height = app.viewport_height_for(&terminal)?;
                        app.handle_session_event_from_thread(thread_id, event, viewport_height);
                        terminal.draw(|frame| app.render(frame))?;
                    }
                    Some(InProcessServerEvent::ServerRequest(request)) => {
                        let viewport_height = app.viewport_height_for(&terminal)?;
                        if matches!(
                            app.handle_server_request(&mut app_server, request, viewport_height).await?,
                            crate::app::AppRunControl::Exit
                        ) {
                            break;
                        }
                        terminal.draw(|frame| app.render(frame))?;
                    }
                    None => break,
                }
            }
            maybe_event = event_stream.next() => {
                match maybe_event {
                    Some(Ok(event)) => {
                        let viewport_height = app.viewport_height_for(&terminal)?;
                        match app.handle_terminal_event(&mut app_server, event, viewport_height).await? {
                            crate::app::AppRunControl::Exit => break,
                            crate::app::AppRunControl::OpenEditor(path) => {
                                edit_plan_in_editor(&mut terminal, &mut app, &path).await?;
                            }
                            crate::app::AppRunControl::Continue => {}
                        }
                        terminal.draw(|frame| app.render(frame))?;
                    }
                    Some(Err(err)) => return Err(err.into()),
                    None => break,
                }
            }
            _ = signals.recv() => break,
        }
    }

    // Single exit epilogue for every break path: shut the core down
    // gracefully — cancel running turns, cascade to child agents, kill tool
    // subprocess groups — before giving the terminal back.
    shutdown_app_server(&mut app_server).await;
    restore(Some(&mut terminal))
}

/// Reject configuration the render layer can't honor before terminal setup.
/// The highlight theme is validated here (not in the leaf config crate, which
/// is two-face-free); `highlight::theme` still falls back defensively.
fn validate_tui_config(config: &cazean_config::Config) -> TuiResult<()> {
    let theme = &config.tui.highlight_theme;
    if highlight::embedded_theme_name(theme).is_none() {
        return Err(TuiError::Config(format!(
            "unknown highlight_theme '{theme}' (set tui.highlight_theme to a known two-face theme, e.g. \"CatppuccinMocha\")"
        )));
    }
    Ok(())
}

/// Bounded graceful shutdown: a hung core must never wedge process exit.
async fn shutdown_app_server(app_server: &mut AppServerSession) {
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), app_server.shutdown()).await;
    // The shutdown request normally fires pending kill sweeps itself; repeat
    // here for the timeout path — if the core hung and we are exiting anyway,
    // outstanding subprocess groups must still be SIGKILLed before the
    // process (and the detached sweep tasks with it) goes away.
    cazean_core::sweep_pending_process_kills();
}

/// SIGINT/SIGTERM listener (no-op stream on non-unix platforms).
struct TerminateSignals {
    #[cfg(unix)]
    sigint: tokio::signal::unix::Signal,
    #[cfg(unix)]
    sigterm: tokio::signal::unix::Signal,
}

impl TerminateSignals {
    fn new() -> TuiResult<Self> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            Ok(Self {
                sigint: signal(SignalKind::interrupt())?,
                sigterm: signal(SignalKind::terminate())?,
            })
        }
        #[cfg(not(unix))]
        Ok(Self {})
    }

    async fn recv(&mut self) {
        #[cfg(unix)]
        {
            tokio::select! {
                _ = self.sigint.recv() => {}
                _ = self.sigterm.recv() => {}
            }
        }
        #[cfg(not(unix))]
        std::future::pending::<()>().await
    }
}

fn init() -> TuiResult<Option<AppTerminal>> {
    if !std::io::stdout().is_terminal() {
        return Ok(None);
    }

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    enter_screen(&mut stdout)?;
    let backend = CrosstermBackend::new(stdout);
    Ok(Some(Terminal::new(backend)?))
}

/// Enter the alternate screen and enable the input modes the UI relies on.
/// Shared by `init` and editor `resume` so the two can never drift.
fn enter_screen<W: std::io::Write>(out: &mut W) -> TuiResult<()> {
    execute!(
        out,
        EnterAlternateScreen,
        EnableBracketedPaste,
        // Alternate scroll mode (DECSET 1007): the terminal turns mouse wheel
        // ticks into Up/Down arrow keys while on the alternate screen. This
        // gives wheel scrolling without mouse capture, so native drag-to-select
        // and copy keep working.
        Print(concat!('\x1b', "[?1007h")),
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
    )?;
    Ok(())
}

/// Reverse of [`enter_screen`]. Shared by `restore` and editor `suspend`.
fn leave_screen<W: std::io::Write>(out: &mut W) -> TuiResult<()> {
    execute!(
        out,
        PopKeyboardEnhancementFlags,
        Print(concat!('\x1b', "[?1007l")),
        DisableBracketedPaste,
        LeaveAlternateScreen
    )?;
    Ok(())
}

fn restore(terminal: Option<&mut AppTerminal>) -> TuiResult<()> {
    let Some(terminal) = terminal else {
        return Ok(());
    };

    disable_raw_mode()?;
    leave_screen(terminal.backend_mut())?;
    terminal.show_cursor()?;
    Ok(())
}

/// Hand the terminal back to a child process: leave the alternate screen and
/// raw mode (and pop the kitty flags) so an external editor behaves normally.
fn suspend(terminal: &mut AppTerminal) -> TuiResult<()> {
    disable_raw_mode()?;
    leave_screen(terminal.backend_mut())?;
    terminal.show_cursor()?;
    Ok(())
}

/// Re-take the terminal after a suspended child exits. Re-pushing the kitty
/// flags is load-bearing (Ctrl-chords break otherwise), and `clear` invalidates
/// ratatui's cached buffer so the next draw repaints over the editor's output.
fn resume(terminal: &mut AppTerminal) -> TuiResult<()> {
    enable_raw_mode()?;
    enter_screen(terminal.backend_mut())?;
    terminal.clear()?;
    Ok(())
}

/// Run the user's editor (`$VISUAL`, then `$EDITOR`, then `vi`) on `path`,
/// waiting for it to exit. `$VISUAL`/`$EDITOR` may carry arguments.
async fn run_editor(path: &std::path::Path) -> std::io::Result<std::process::ExitStatus> {
    let editor = std::env::var("VISUAL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            std::env::var("EDITOR")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .unwrap_or_else(|| "vi".to_string());
    let mut parts = editor.split_whitespace();
    let program = parts.next().unwrap_or("vi");
    let args: Vec<&str> = parts.collect();
    tokio::process::Command::new(program)
        .args(args)
        .arg(path)
        .status()
        .await
}

/// Suspend the TUI, edit `path` in `$EDITOR`, restore the terminal, and refresh
/// the approval overlay with whatever was saved.
async fn edit_plan_in_editor(
    terminal: &mut AppTerminal,
    app: &mut App,
    path: &std::path::Path,
) -> TuiResult<()> {
    suspend(terminal)?;
    let result = run_editor(path).await;
    resume(terminal)?;
    if let Err(err) = result {
        tracing::warn!(error = %err, "failed to launch editor for plan");
    }
    app.reload_plan_after_edit();
    Ok(())
}
