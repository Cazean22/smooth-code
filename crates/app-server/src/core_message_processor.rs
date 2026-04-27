use std::collections::HashSet;

use app_server_protocol::{
    ClientRequest, ThreadListItem, ThreadListResponse, ThreadResumeResponse, ThreadStartResponse,
    TurnStartResponse,
};
use smooth_core::{ThreadManagerState, ThreadSummary};
use smooth_protocol::ThreadId;
use tokio::sync::{Mutex, mpsc};

use crate::in_process::InProcessServerEvent;

pub(crate) struct CoreMessageProcessor {
    threads: ThreadManagerState,
    event_tx: mpsc::Sender<InProcessServerEvent>,
    subscribed_threads: Mutex<HashSet<ThreadId>>,
}

impl CoreMessageProcessor {
    pub fn new(event_tx: mpsc::Sender<InProcessServerEvent>) -> Self {
        Self {
            threads: ThreadManagerState::new(),
            event_tx,
            subscribed_threads: Mutex::new(HashSet::new()),
        }
    }

    pub async fn process_request(
        &self,
        request: ClientRequest,
    ) -> Result<serde_json::Value, app_server_protocol::JSONRPCErrorError> {
        match request {
            ClientRequest::ThreadStart { .. } => {
                let started = self
                    .threads
                    .start_thread()
                    .await
                    .map_err(internal_error)?;
                self.ensure_thread_subscription(started.thread_id).await;
                self.threads
                    .emit_session_configured(started.thread_id)
                    .await
                    .map_err(internal_error)?;
                serde_json::to_value(ThreadStartResponse {
                    thread_id: started.thread_id.to_string(),
                    rollout_path: started.rollout_path.display().to_string(),
                })
                .map_err(internal_serde_error)
            }
            ClientRequest::TurnStart { params, .. } => {
                let thread_id = params.thread_id.parse::<ThreadId>().map_err(|err| {
                    app_server_protocol::JSONRPCErrorError {
                        code: -32602,
                        data: None,
                        message: format!("invalid thread id: {err}"),
                    }
                })?;
                self.ensure_thread_subscription(thread_id).await;
                let turn_id = self
                    .threads
                    .start_user_input(thread_id, params.input)
                    .await
                    .map_err(internal_error)?;
                serde_json::to_value(TurnStartResponse {
                    thread_id: thread_id.to_string(),
                    turn_id,
                })
                .map_err(internal_serde_error)
            }
            ClientRequest::ThreadResume { params, .. } => {
                let thread_id = params.thread_id.parse::<ThreadId>().map_err(|err| {
                    app_server_protocol::JSONRPCErrorError {
                        code: -32602,
                        data: None,
                        message: format!("invalid thread id: {err}"),
                    }
                })?;
                let resumed = self
                    .threads
                    .resume_thread(thread_id)
                    .await
                    .map_err(internal_error)?;
                self.ensure_thread_subscription(thread_id).await;
                serde_json::to_value(ThreadResumeResponse {
                    thread_id: resumed.thread_id.to_string(),
                    rollout_path: resumed.rollout_path.display().to_string(),
                    initial_messages: resumed.initial_messages,
                })
                .map_err(internal_serde_error)
            }
            ClientRequest::ThreadList { params, .. } => {
                let threads = self.threads.list_threads().await.map_err(internal_error)?;
                let offset = params
                    .cursor
                    .as_deref()
                    .and_then(|cursor| cursor.parse::<usize>().ok())
                    .unwrap_or(0);
                let limit = params.limit.unwrap_or(20) as usize;
                let page = threads
                    .into_iter()
                    .skip(offset)
                    .take(limit)
                    .map(map_thread_summary)
                    .collect::<Vec<_>>();
                let next_cursor = (page.len() == limit).then(|| (offset + limit).to_string());
                serde_json::to_value(ThreadListResponse {
                    data: page,
                    next_cursor,
                })
                .map_err(internal_serde_error)
            }
        }
    }

    async fn ensure_thread_subscription(&self, thread_id: ThreadId) {
        {
            let mut subscribed = self.subscribed_threads.lock().await;
            if !subscribed.insert(thread_id) {
                return;
            }
        }

        let Ok(mut rx) = self.threads.subscribe(thread_id).await else {
            return;
        };
        let event_tx = self.event_tx.clone();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        if event_tx
                            .send(InProcessServerEvent::SessionEvent(event))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }
}

fn internal_error(err: anyhow::Error) -> app_server_protocol::JSONRPCErrorError {
    app_server_protocol::JSONRPCErrorError {
        code: -32000,
        data: None,
        message: err.to_string(),
    }
}

fn internal_serde_error(err: serde_json::Error) -> app_server_protocol::JSONRPCErrorError {
    app_server_protocol::JSONRPCErrorError {
        code: -32603,
        data: None,
        message: err.to_string(),
    }
}

fn map_thread_summary(summary: ThreadSummary) -> ThreadListItem {
    ThreadListItem {
        thread_id: summary.thread_id.to_string(),
        rollout_path: summary.rollout_path.display().to_string(),
        created_at: summary.created_at,
        updated_at: summary.updated_at,
        last_user_message: summary.last_user_message,
        last_assistant_message: summary.last_assistant_message,
    }
}
