use std::collections::HashSet;

use app_server_protocol::{ClientRequest, ThreadStartResponse, TurnStartResponse};
use smooth_core::ThreadManagerState;
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
                let started = self.threads.start_thread().await.map_err(|err| {
                    app_server_protocol::JSONRPCErrorError {
                        code: -32000,
                        data: None,
                        message: err.to_string(),
                    }
                })?;
                self.ensure_thread_subscription(started.thread_id).await;
                self.threads
                    .emit_session_configured(started.thread_id)
                    .await
                    .map_err(|err| app_server_protocol::JSONRPCErrorError {
                        code: -32000,
                        data: None,
                        message: err.to_string(),
                    })?;
                serde_json::to_value(ThreadStartResponse {
                    thread_id: started.thread_id.to_string(),
                })
                .map_err(|err| app_server_protocol::JSONRPCErrorError {
                    code: -32603,
                    data: None,
                    message: err.to_string(),
                })
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
                    .map_err(|err| app_server_protocol::JSONRPCErrorError {
                        code: -32000,
                        data: None,
                        message: err.to_string(),
                    })?;
                serde_json::to_value(TurnStartResponse {
                    thread_id: thread_id.to_string(),
                    turn_id,
                })
                .map_err(|err| app_server_protocol::JSONRPCErrorError {
                    code: -32603,
                    data: None,
                    message: err.to_string(),
                })
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
