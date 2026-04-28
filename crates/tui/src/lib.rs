mod app;
mod app_server_client;
mod app_server_session;
mod history_cell;
mod markdown;
mod markdown_render;
mod markdown_stream;
mod streaming;

use std::io::{IsTerminal, Stdout};

use crate::{app::App, app_server_client::AppServerClient, app_server_session::AppServerSession};
use anyhow::Result;
use app_server::in_process::InProcessServerEvent;
use crossterm::{
    event::{DisableBracketedPaste, EnableBracketedPaste},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures_util::StreamExt;
use ratatui::{Terminal, prelude::CrosstermBackend};

pub type AppTerminal = Terminal<CrosstermBackend<Stdout>>;

#[tracing::instrument(name = "tui.run", skip_all)]
pub async fn run() -> Result<()> {
    let mut terminal = init()?.ok_or_else(|| anyhow::anyhow!("smooth-tui requires a TTY"))?;
    let mut app_server = AppServerSession::new(AppServerClient::start(512)?);
    let mut app = App::new();
    let started_thread = app_server.start_thread().await?;
    app.current_thread_id = Some(started_thread.thread_id.parse()?);
    let mut event_stream = crossterm::event::EventStream::new();
    terminal.draw(|frame| app.render(frame))?;

    loop {
        tokio::select! {
            event = app_server.next_event() => {
                match event {
                    Some(InProcessServerEvent::SessionEvent(event)) => {
                        let viewport_height = app.viewport_height_for(&terminal)?;
                        app.handle_session_event(event, viewport_height);
                        terminal.draw(|frame| app.render(frame))?;
                    }
                    Some(InProcessServerEvent::ServerRequest(_)) => {}
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

fn init() -> Result<Option<AppTerminal>> {
    if !std::io::stdout().is_terminal() {
        return Ok(None);
    }

    enable_raw_mode()?;

    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste,)?;

    let backend = CrosstermBackend::new(stdout);
    Ok(Some(Terminal::new(backend)?))
}

fn restore(terminal: Option<&mut AppTerminal>) -> Result<()> {
    let Some(terminal) = terminal else {
        return Ok(());
    };

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableBracketedPaste,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    Ok(())
}
