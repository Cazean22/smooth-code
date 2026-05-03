use std::{collections::HashMap, sync::Arc};

use app_server_protocol::{ClientCommand, JSONRPCErrorError};
use smooth_protocol::Event;
use tokio::sync::mpsc;
use tracing::Instrument;

use crate::{
    OutboundConnectionState,
    error_code::{OVERLOADED_ERROR_CODE, SERVER_ERROR_CODE},
    message_processor::MessageProcessor,
    outgoing_message::{
        ConnectionId, OutgoingMessage, OutgoingMessageSender, QueuedOutgoingMessage,
    },
    route_outgoing_envelope,
};

pub(crate) const IN_PROCESS_CONNECTION_ID: ConnectionId = ConnectionId(0);

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

pub fn start(args: InProcessStartArgs) -> InProcessClientHandle {
    start_internal(args).0
}

fn start_internal(args: InProcessStartArgs) -> (InProcessClientHandle, Arc<OutgoingMessageSender>) {
    let channel_capacity = args.channel_capacity.max(1);
    let (client_tx, mut client_rx) = mpsc::channel::<ClientCommand>(channel_capacity);
    let (event_tx, event_rx) = mpsc::channel::<InProcessServerEvent>(channel_capacity);
    let (outgoing_tx, mut outgoing_rx) = mpsc::channel(channel_capacity);
    let outgoing_message_sender = Arc::new(OutgoingMessageSender::new(outgoing_tx));
    let runtime_outgoing = Arc::clone(&outgoing_message_sender);

    let runtime_span = tracing::info_span!("app_server.in_process_runtime", channel_capacity);
    let runtime_handle = tokio::task::Builder::new()
        .name("app_server.in_process.runtime")
        .spawn(
            async move {
                let (writer_tx, mut writer_rx) =
                    mpsc::channel::<QueuedOutgoingMessage>(channel_capacity);
                let mut outbound_connections = HashMap::<ConnectionId, OutboundConnectionState>::new();
                outbound_connections.insert(
                    IN_PROCESS_CONNECTION_ID,
                    OutboundConnectionState::new(writer_tx, None),
                );

                let outbound_handle = tokio::task::Builder::new()
                    .name("app_server.outbound_router")
                    .spawn(async move {
                        while let Some(envelope) = outgoing_rx.recv().await {
                            route_outgoing_envelope(&mut outbound_connections, envelope).await;
                        }
                    })
                    .expect("failed to spawn app-server outbound router");

                let processor_outgoing = Arc::clone(&runtime_outgoing);
                let processor_event_tx = event_tx.clone();
                let (processor_tx, mut processor_rx) =
                    mpsc::channel::<ClientCommand>(channel_capacity);
                let processor_handle = tokio::task::Builder::new()
                    .name("app_server.message_processor")
                    .spawn(async move {
                        let processor = match MessageProcessor::new(
                            processor_event_tx,
                            processor_outgoing,
                        )
                        .await
                        {
                            Ok(processor) => Arc::new(processor),
                            Err(err) => {
                                tracing::error!(error = %err, "failed to initialize message processor");
                                return;
                            }
                        };

                        loop {
                            match processor_rx.recv().await {
                                Some(ClientCommand::Request { request, response_tx }) => {
                                    processor
                                        .process_client_request(*request, response_tx)
                                        .await;
                                }
                                Some(
                                    ClientCommand::ServerRequestResponse { .. }
                                    | ClientCommand::ServerRequestError { .. },
                                ) => {
                                    tracing::warn!(
                                        "received server request completion command on processor queue"
                                    );
                                }
                                None => break,
                            }
                        }
                    })
                    .expect("failed to spawn app-server message processor");

                loop {
                    tokio::select! {
                        message = client_rx.recv() => {
                            match message {
                                Some(command @ ClientCommand::Request { .. }) => {
                                    if processor_tx.send(command).await.is_err() {
                                        break;
                                    }
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
                        queued_message = writer_rx.recv() => {
                            let Some(queued_message) = queued_message else {
                                break;
                            };
                            let write_complete_tx = queued_message.write_complete_tx;
                            match queued_message.message {
                                OutgoingMessage::Request(request) => {
                                    let request_id = request.id().clone();
                                    if let Err(send_error) = event_tx.try_send(
                                        InProcessServerEvent::ServerRequest(request),
                                    ) {
                                        let (code, message) = match send_error {
                                            mpsc::error::TrySendError::Full(_) => (
                                                OVERLOADED_ERROR_CODE,
                                                "in-process server request queue is full",
                                            ),
                                            mpsc::error::TrySendError::Closed(_) => (
                                                SERVER_ERROR_CODE,
                                                "in-process server request consumer is closed",
                                            ),
                                        };
                                        runtime_outgoing
                                            .notify_client_error(
                                                request_id,
                                                JSONRPCErrorError {
                                                    code,
                                                    data: None,
                                                    message: message.to_string(),
                                                },
                                            )
                                            .await;
                                    }
                                }
                                OutgoingMessage::Response(response) => {
                                    tracing::warn!(
                                        request_id = ?response.id,
                                        "dropping unexpected in-process outgoing response"
                                    );
                                }
                                OutgoingMessage::Error(error) => {
                                    tracing::warn!(
                                        request_id = ?error.id,
                                        "dropping unexpected in-process outgoing error"
                                    );
                                }
                            }
                            if let Some(write_complete_tx) = write_complete_tx {
                                let _ = write_complete_tx.send(());
                            }
                        }
                    }
                }

                drop(writer_rx);
                drop(processor_tx);
                runtime_outgoing
                    .cancel_all_requests(Some(JSONRPCErrorError {
                        code: SERVER_ERROR_CODE,
                        data: None,
                        message: "in-process app-server runtime is shutting down".to_string(),
                    }))
                    .await;
                drop(runtime_outgoing);
                let _ = outbound_handle.await;
                let _ = processor_handle.await;
            }
            .instrument(runtime_span),
        )
        .expect("failed to spawn app-server runtime");

    (
        InProcessClientHandle {
            client_tx,
            event_rx,
            runtime_handle,
        },
        outgoing_message_sender,
    )
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
        });
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
