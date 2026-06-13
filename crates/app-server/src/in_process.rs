use std::sync::Arc;

use app_server_protocol::{ClientCommand, JsonRpcError};
use smooth_config::Config;
use smooth_protocol::{ErrorInfo, Event, ThreadId};
use tokio::sync::mpsc;
use tracing::Instrument;

use crate::{
    error::{AppServerError, AppServerResult},
    error_code::SERVER_ERROR_CODE,
    message_processor::MessageProcessor,
    outgoing_message::OutgoingMessageSender,
};

#[derive(Clone)]
pub struct InProcessStartArgs {
    pub channel_capacity: usize,
    pub config: Arc<Config>,
}

impl InProcessStartArgs {
    pub fn new(channel_capacity: usize, config: Arc<Config>) -> Self {
        Self {
            channel_capacity,
            config,
        }
    }
}

#[derive(Debug, Clone)]
pub enum InProcessServerEvent {
    ServerRequest(app_server_protocol::ServerRequest),
    SessionEvent { thread_id: ThreadId, event: Event },
}

pub struct InProcessClientHandle {
    pub client_tx: mpsc::Sender<ClientCommand>,
    event_rx: mpsc::Receiver<InProcessServerEvent>,
    #[allow(dead_code)]
    runtime_handle: tokio::task::JoinHandle<()>,
}

impl InProcessClientHandle {
    pub async fn next_event(&mut self) -> Option<InProcessServerEvent> {
        self.event_rx.recv().await
    }
}

pub async fn start(args: InProcessStartArgs) -> AppServerResult<InProcessClientHandle> {
    Ok(start_internal(args).await?.0)
}

async fn start_internal(
    args: InProcessStartArgs,
) -> AppServerResult<(InProcessClientHandle, Arc<OutgoingMessageSender>)> {
    let channel_capacity = args.channel_capacity.max(1);
    let (client_tx, mut client_rx) = mpsc::channel::<ClientCommand>(channel_capacity);
    let (event_tx, event_rx) = mpsc::channel::<InProcessServerEvent>(channel_capacity);
    let outgoing_message_sender = Arc::new(OutgoingMessageSender::new(event_tx.clone()));
    let runtime_outgoing = Arc::clone(&outgoing_message_sender);
    let processor = Arc::new(
        MessageProcessor::new(event_tx.clone(), Arc::clone(&runtime_outgoing), args.config)
            .await
            .map_err(|err| {
                AppServerError::Internal(format!("failed to initialize message processor: {err}"))
            })?,
    );

    let runtime_span = tracing::info_span!("app_server.in_process_runtime", channel_capacity);
    let runtime_handle = tokio::task::Builder::new()
        .name("app_server.in_process.runtime")
        .spawn(
            async move {
                loop {
                    tokio::select! {
                        message = client_rx.recv() => {
                            match message {
                                Some(ClientCommand::Request { request, response_tx }) => {
                                    let processor = Arc::clone(&processor);
                                    tokio::spawn(async move {
                                        processor
                                            .process_client_request(*request, response_tx)
                                            .await;
                                    });
                                }
                                Some(ClientCommand::ServerRequestResponse { request_id, result }) => {
                                    runtime_outgoing
                                        .notify_client_response(request_id, result)
                                        .await;
                                }
                                Some(ClientCommand::ServerRequestError { request_id, error }) => {
                                    runtime_outgoing
                                        .notify_client_error(request_id, error)
                                        .await;
                                }
                                None => break,
                            }
                        }
                    }
                }

                runtime_outgoing
                    .cancel_all_requests(Some(JsonRpcError::new(
                        SERVER_ERROR_CODE,
                        ErrorInfo::new(
                            "runtime_shutdown",
                            "in-process app-server runtime is shutting down",
                        )
                        .with_source("app-server"),
                    )))
                    .await;
                drop(runtime_outgoing);
            }
            .instrument(runtime_span),
        )
        .map_err(|source| AppServerError::TaskSpawn {
            task_name: "app_server.in_process.runtime",
            source,
        })?;

    Ok((
        InProcessClientHandle {
            client_tx,
            event_rx,
            runtime_handle,
        },
        outgoing_message_sender,
    ))
}

#[cfg(test)]
mod tests {
    use app_server_protocol::{AskUserQuestionParams, ServerRequestPayload};
    use serde_json::json;
    use smooth_protocol::ThreadId;
    use tokio::time::{Duration, timeout};

    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[tokio::test]
    async fn server_request_round_trip_resolves_waiter() -> TestResult {
        // The processor opens the state DB under the cwd; hold the crate-wide
        // cwd lock and pin a temp cwd so cwd-switching tests in other modules
        // cannot interfere.
        let _cwd_guard = crate::cwd_test_lock().lock().await;
        let workspace = tempfile::TempDir::new()?;
        let original_cwd = std::env::current_dir()?;
        std::env::set_current_dir(workspace.path())?;

        let result = run_round_trip().await;
        std::env::set_current_dir(original_cwd)?;
        result
    }

    async fn run_round_trip() -> TestResult {
        let (mut handle, outgoing) =
            start_internal(InProcessStartArgs::new(8, Arc::new(Config::default()))).await?;
        let thread_id = ThreadId::new();
        let (request_id, response_rx) = outgoing
            .send_request(ServerRequestPayload::AskUserQuestion(
                AskUserQuestionParams {
                    thread_id: thread_id.to_string(),
                    turn_id: "turn-1".to_string(),
                    questions: Vec::new(),
                },
            ))
            .await;

        let event = timeout(Duration::from_secs(1), handle.next_event())
            .await?
            .ok_or("runtime event stream should stay open")?;
        let observed_request_id = match event {
            InProcessServerEvent::ServerRequest(
                app_server_protocol::ServerRequest::AskUserQuestion { request_id, .. },
            ) => request_id,
            other => panic!("unexpected event: {other:?}"),
        };
        assert_eq!(observed_request_id, request_id);

        handle
            .client_tx
            .send(ClientCommand::ServerRequestResponse {
                request_id: observed_request_id.clone(),
                result: json!({ "ok": true }),
            })
            .await
            .map_err(|err| err.to_string())?;

        let result = timeout(Duration::from_secs(1), response_rx).await???;
        assert_eq!(result, json!({ "ok": true }));
        Ok(())
    }
}
