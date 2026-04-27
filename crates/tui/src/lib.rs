mod app;
mod app_event;
mod app_server_client;
mod app_server_session;

use std::io::{IsTerminal, Stdout};

use crate::{
    app::App, app_event::AppEvent, app_server_client::AppServerClient,
    app_server_session::AppServerSession,
};
use anyhow::Result;
use app_server::in_process::InProcessServerEvent;
use crossterm::{
    event::{DisableBracketedPaste, EnableBracketedPaste},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, prelude::CrosstermBackend};
use smooth_protocol::Op;
use tokio::sync::mpsc::unbounded_channel;

pub type AppTerminal = Terminal<CrosstermBackend<Stdout>>;

pub async fn run() -> Result<()> {
    let mut terminal = init()?;
    let (app_event_tx, mut app_event_rx) = unbounded_channel();
    let mut app_server = AppServerSession::new(AppServerClient::start(512)?);
    let mut app = App {
        app_event_tx: app_event_tx.clone(),
        current_thread_id: None,
    };
    let started_thread = app_server.start_thread().await?;
    app.current_thread_id = Some(started_thread.thread_id.parse()?);
    tokio::spawn(async move {
        let _ = app_event_tx.send(AppEvent::SubmitThreadOp {
            op: Op::UserInput("Hi".to_owned()),
        });
    });

    loop {
        tokio::select! {
            event = app_event_rx.recv() => {
                if let Some(event) = event {
                    let _ = app.handle_event(&mut app_server, event).await;
                } else {
                    break;
                }
            }
            event = app_server.next_event() => {
                match event {
                    Some(InProcessServerEvent::SessionEvent(event)) => app.handle_session_event(event),
                    Some(InProcessServerEvent::ServerRequest(_)) => {}
                    None => break,
                }
            }
        }
    }

    restore(terminal.as_mut())
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
