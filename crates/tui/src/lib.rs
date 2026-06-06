#![deny(clippy::unwrap_used, clippy::expect_used)]

mod app;
mod app_server_client;
mod app_server_session;
mod diff_render;
mod error;
mod highlight;
mod history_cell;
mod markdown;
mod markdown_render;
mod markdown_stream;
mod project_instructions;
mod question_picker;
mod streaming;
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
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures_util::StreamExt;
use ratatui::{Terminal, prelude::CrosstermBackend};

pub use error::{TuiError, TuiResult};

pub type AppTerminal = Terminal<CrosstermBackend<Stdout>>;

#[tracing::instrument(name = "tui.run", skip_all)]
pub async fn run() -> TuiResult<()> {
    let mut terminal = init()?.ok_or(TuiError::TtyRequired)?;
    let mut app_server = AppServerSession::new(AppServerClient::start(512).await?);
    let mut app = App::new();
    let mut event_stream = crossterm::event::EventStream::new();
    terminal.draw(|frame| app.render(frame))?;
    let viewport_height = app.viewport_height_for(&terminal)?;
    if matches!(
        app.startup(&mut app_server, viewport_height).await?,
        crate::app::AppRunControl::Exit
    ) {
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
                        if matches!(
                            app.handle_terminal_event(&mut app_server, event, viewport_height).await?,
                            crate::app::AppRunControl::Exit
                        ) {
                            break;
                        }
                        terminal.draw(|frame| app.render(frame))?;
                    }
                    Some(Err(err)) => return Err(err.into()),
                    None => break,
                }
            }
        }
    }

    restore(Some(&mut terminal))
}

fn init() -> TuiResult<Option<AppTerminal>> {
    if !std::io::stdout().is_terminal() {
        return Ok(None);
    }

    enable_raw_mode()?;

    let mut stdout = std::io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableBracketedPaste,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
    )?;

    let backend = CrosstermBackend::new(stdout);
    Ok(Some(Terminal::new(backend)?))
}

fn restore(terminal: Option<&mut AppTerminal>) -> TuiResult<()> {
    let Some(terminal) = terminal else {
        return Ok(());
    };

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        PopKeyboardEnhancementFlags,
        DisableBracketedPaste,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    Ok(())
}
