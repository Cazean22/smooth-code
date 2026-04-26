use anyhow::Result;
use smooth_protocol::{Op, ThreadId};
use tokio::sync::mpsc;

use crate::{app_event::AppEvent, app_server_session::AppServerSession};

#[derive(Debug)]
pub(crate) enum AppRunControl {
    Continue,
    Exit(ExitReason),
}

#[derive(Debug, Clone)]
pub enum ExitReason {
    UserRequested,
    Fatal(String),
}

pub(crate) struct App {
    pub(crate) app_event_tx: mpsc::UnboundedSender<AppEvent>,
}

impl App {
    pub(crate) async fn handle_event(
        &mut self,
        app_server: &mut AppServerSession,
        event: AppEvent,
    ) -> Result<AppRunControl> {
        match event {
            AppEvent::SubmitThreadOp { thread_id, op } => {
                self.submit_thread_op(app_server, thread_id, op).await?;
            }
        }
        Ok(AppRunControl::Continue)
    }
    async fn submit_thread_op(
        &mut self,
        app_server: &mut AppServerSession,
        thread_id: ThreadId,
        op: Op,
    ) -> Result<()> {
        match op {
            Op::UserInput(input) => {
                let _response = app_server.turn_start(thread_id, input).await?;
            }
        }
        Ok(())
    }
}
