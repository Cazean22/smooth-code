use std::{
    collections::HashSet,
    sync::{Arc, atomic::AtomicBool},
};

use app_server_protocol::{ClientRequest, JSONRPCErrorError, ServerRequest};
use smooth_protocol::Event;
use tokio::sync::{RwLock, mpsc, oneshot};
use tracing::Instrument;

use crate::{
    OutboundConnectionState,
    message_processor::{ConnectionSessionState, MessageProcessor},
    outgoing_message::{OutgoingEnvelope, OutgoingMessageSender, QueuedOutgoingMessage},
    route_outgoing_envelope,
};

#[derive(Clone)]
pub struct InProcessStartArgs {
    /// Capacity used for all runtime queues (clamped to at least 1).
    pub channel_capacity: usize,
}
pub enum InProcessClientMessage {
    Request {
        request: Box<ClientRequest>,
        response_tx: oneshot::Sender<std::result::Result<serde_json::Value, JSONRPCErrorError>>,
    },
}
enum ProcessorCommand {
    Request {
        request: Box<ClientRequest>,
        response_tx: oneshot::Sender<std::result::Result<serde_json::Value, JSONRPCErrorError>>,
    },
}

/// Event emitted from the app-server to the in-process client.
///
/// event — it signals that the consumer fell behind and some events were dropped.
#[derive(Debug, Clone)]
pub enum InProcessServerEvent {
    /// Server request that requires client response/rejection.
    ServerRequest(ServerRequest),
    SessionEvent(Event),
}

pub struct InProcessClientHandle {
    pub client_tx: mpsc::Sender<InProcessClientMessage>,
    event_rx: mpsc::Receiver<InProcessServerEvent>,
    runtime_handle: tokio::task::JoinHandle<()>,
}

impl InProcessClientHandle {
    pub async fn next_event(&mut self) -> Option<InProcessServerEvent> {
        self.event_rx.recv().await
    }
}

pub fn start(args: InProcessStartArgs) -> InProcessClientHandle {
    let channel_capacity = args.channel_capacity.max(1);
    let (client_tx, mut client_rx) = mpsc::channel::<InProcessClientMessage>(channel_capacity);
    let (event_tx, event_rx) = mpsc::channel::<InProcessServerEvent>(channel_capacity);

    let runtime_span = tracing::info_span!(
        "app_server.in_process_runtime",
        channel_capacity,
    );
    let runtime_handle = tokio::task::Builder::new()
        .name("app_server.in_process.runtime")
        .spawn(
            async move {
                let (outgoing_tx, mut outgoing_rx) = mpsc::channel::<OutgoingEnvelope>(channel_capacity);
                let outgoing_message_sender = Arc::new(OutgoingMessageSender::new(outgoing_tx));

                let (writer_tx, mut writer_rx) = mpsc::channel::<QueuedOutgoingMessage>(channel_capacity);
                let outbound_initialized = Arc::new(AtomicBool::new(false));
                let outbound_opted_out_notification_methods = Arc::new(RwLock::new(HashSet::new()));

                let outbound_connection_state = OutboundConnectionState::new(
                    writer_tx,
                    Arc::clone(&outbound_initialized),
                    Arc::clone(&outbound_opted_out_notification_methods),
                    /*disconnect_sender*/ None,
                );

                let outbound_handle = tokio::task::Builder::new()
                    .name("app_server.outbound_router")
                    .spawn(async move {
                        while let Some(envelope) = outgoing_rx.recv().await {
                            route_outgoing_envelope(&outbound_connection_state, envelope).await;
                        }
                    })
                    .expect("failed to spawn app-server outbound router");

                let (processor_tx, mut processor_rx) =
                    mpsc::channel::<ProcessorCommand>(channel_capacity);
                let processor_handle = tokio::task::Builder::new()
                    .name("app_server.message_processor")
                    .spawn(async move {
                        let processor = Arc::new(MessageProcessor::new(event_tx));
                        let session = Arc::new(ConnectionSessionState::default());

                        loop {
                            tokio::select! {
                                command = processor_rx.recv() => {
                                    match command {
                                        Some(ProcessorCommand::Request { request, response_tx }) => {
                                            processor
                                                .process_client_request(
                                                    *request,
                                                    Arc::clone(&session),
                                                    &outbound_initialized,
                                                    response_tx,
                                                )
                                                .await;
                                        }
                                        None => {
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                    })
                    .expect("failed to spawn app-server message processor");
                loop {
                    tokio::select! {
                        message = client_rx.recv() => {
                            match message {
                                Some(InProcessClientMessage::Request { request, response_tx }) => {
                                    if processor_tx
                                        .send(ProcessorCommand::Request { request, response_tx })
                                        .await
                                        .is_err()
                                    {
                                        break;
                                    }
                                }
                                None => break,
                            }
                        }
                        queued_message = writer_rx.recv() => {
                            if queued_message.is_none() {
                                break;
                            }
                        }
                    }
                }
                drop(writer_rx);
                drop(processor_tx);
                drop(outgoing_message_sender);
                let _ = outbound_handle.await;
                let _ = processor_handle.await;
            }
            .instrument(runtime_span),
        )
        .expect("failed to spawn app-server runtime");
    InProcessClientHandle {
        client_tx,
        event_rx,
        runtime_handle,
    }
}
