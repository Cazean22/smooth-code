use anyhow::Result;
use smooth_protocol::{Event, EventMsg, Op, ThreadId};
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
    pub(crate) current_thread_id: Option<ThreadId>,
}

impl App {
    pub(crate) async fn handle_event(
        &mut self,
        app_server: &mut AppServerSession,
        event: AppEvent,
    ) -> Result<AppRunControl> {
        match event {
            AppEvent::SubmitThreadOp { op } => {
                self.submit_thread_op(app_server, op).await?;
            }
        }
        Ok(AppRunControl::Continue)
    }
    async fn submit_thread_op(&mut self, app_server: &mut AppServerSession, op: Op) -> Result<()> {
        let thread_id = self
            .current_thread_id
            .ok_or_else(|| anyhow::anyhow!("no started thread available for prompt submission"))?;
        match op {
            Op::UserInput(input) => {
                let response = app_server.turn_start(thread_id, input).await?;
                println!(
                    "turn started: thread={} turn={}",
                    response.thread_id, response.turn_id
                );
            }
        }
        Ok(())
    }

    pub(crate) fn handle_session_event(&mut self, event: Event) {
        match event.msg {
            EventMsg::SessionConfigured(configured) => {
                match configured.thread_id.parse::<ThreadId>() {
                    Ok(thread_id) => {
                        self.current_thread_id = Some(thread_id);
                    }
                    Err(err) => {
                        eprintln!("error: invalid configured thread id: {err}");
                    }
                }
            }
            EventMsg::AgentMessageDelta(delta) => {
                print!("{}", delta.delta);
            }
            EventMsg::AgentMessageCompleted(completed) => {
                if !completed.text.ends_with('\n') {
                    println!();
                }
            }
            EventMsg::ToolCallStarted(tool) => {
                println!("\n[tool:start] {} {}", tool.tool_name, tool.args_preview);
            }
            EventMsg::ToolCallCompleted(tool) => {
                if let Some(output_preview) = tool.output_preview {
                    println!("[tool:end] {} {}", tool.call_id, output_preview);
                } else if let Some(error) = tool.error {
                    println!("[tool:end] {} error: {}", tool.call_id, error);
                } else {
                    println!("[tool:end] {}", tool.call_id);
                }
            }
            EventMsg::TurnCompleted(turn) => {
                println!(
                    "turn completed: thread={} turn={}",
                    turn.thread_id, turn.turn_id
                );
            }
            EventMsg::TurnInterrupted(turn) => {
                println!(
                    "turn interrupted: thread={} turn={} reason={}",
                    turn.thread_id, turn.turn_id, turn.reason
                );
            }
            EventMsg::Error(error) => {
                eprintln!("error: {}", error.message);
            }
            EventMsg::TurnStarted(_)
            | EventMsg::AgentStatusChanged(_)
            | EventMsg::AgentMessage(_)
            | EventMsg::UserMessage(_) => {}
        }
    }
}
