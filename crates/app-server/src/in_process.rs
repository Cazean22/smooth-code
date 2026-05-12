use std::sync::Arc;

use anyhow::{Context, Result};
use app_server_protocol::{ClientCommand, JSONRPCErrorError};
use smooth_protocol::Event;
use tokio::sync::mpsc;
use tracing::Instrument;

use crate::{
    error_code::SERVER_ERROR_CODE, message_processor::MessageProcessor,
    outgoing_message::OutgoingMessageSender,
};

#[derive(Clone)]
pub struct InProcessStartArgs {
    pub channel_capacity: usize,
}

#[derive(Debug, Clone)]
pub enum InProcessServerEvent {
    ServerRequest(app_server_protocol::ServerRequest),
    SessionEvent(Event),
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

pub async fn start(args: InProcessStartArgs) -> Result<InProcessClientHandle> {
    Ok(start_internal(args).await?.0)
}

async fn start_internal(
    args: InProcessStartArgs,
) -> Result<(InProcessClientHandle, Arc<OutgoingMessageSender>)> {
    let channel_capacity = args.channel_capacity.max(1);
    let (client_tx, mut client_rx) = mpsc::channel::<ClientCommand>(channel_capacity);
    let (event_tx, event_rx) = mpsc::channel::<InProcessServerEvent>(channel_capacity);
    let outgoing_message_sender = Arc::new(OutgoingMessageSender::new(event_tx.clone()));
    let runtime_outgoing = Arc::clone(&outgoing_message_sender);
    let processor = Arc::new(
        MessageProcessor::new(event_tx.clone(), Arc::clone(&runtime_outgoing))
            .await
            .context("failed to initialize message processor")?,
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
                                    tokio::task::Builder::new()
                                        .name("app_server.process_request")
                                        .spawn(async move {
                                            processor
                                                .process_client_request(*request, response_tx)
                                                .await;
                                        })
                                        .expect("failed to spawn request processor task");
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
                    .cancel_all_requests(Some(JSONRPCErrorError {
                        code: SERVER_ERROR_CODE,
                        data: None,
                        message: "in-process app-server runtime is shutting down".to_string(),
                    }))
                    .await;
                drop(runtime_outgoing);
            }
            .instrument(runtime_span),
        )
        .context("failed to spawn app-server runtime")?;

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
    use app_server_protocol::{DynamicToolCallParams, ServerRequestPayload};
    use serde_json::json;
    use smooth_protocol::ThreadId;
    use tokio::time::{Duration, timeout};

    use super::*;

    #[tokio::test]
    async fn server_request_round_trip_resolves_waiter() {
        let (mut handle, outgoing) = start_internal(InProcessStartArgs {
            channel_capacity: 8,
        })
        .await
        .expect("in-process app-server should initialize");
        let thread_id = ThreadId::new();
        let (request_id, response_rx) = outgoing
            .send_request(ServerRequestPayload::DynamicToolCall(
                DynamicToolCallParams {
                    thread_id: thread_id.to_string(),
                    turn_id: "turn-1".to_string(),
                    call_id: "call-1".to_string(),
                    tool: "dynamic_echo".to_string(),
                    arguments: json!({ "message": "hi" }),
                },
            ))
            .await;

        let event = timeout(Duration::from_secs(1), handle.next_event())
            .await
            .expect("runtime should emit server request")
            .expect("runtime event stream should stay open");
        let observed_request_id = match event {
            InProcessServerEvent::ServerRequest(
                app_server_protocol::ServerRequest::DynamicToolCall { request_id, .. },
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
            .expect("client response should send");

        let result = timeout(Duration::from_secs(1), response_rx)
            .await
            .expect("server waiter should resolve")
            .expect("waiter channel should stay open")
            .expect("client should respond successfully");
        assert_eq!(result, json!({ "ok": true }));
    }
}
