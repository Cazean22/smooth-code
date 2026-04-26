use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, atomic::AtomicBool},
};

use app_server_protocol::{ClientRequest, JSONRPCErrorError, RequestId, ServerRequest};
use tokio::sync::{RwLock, mpsc, oneshot};

use crate::{
    ClientRequestResult, OutboundConnectionState,
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
    Request(Box<ClientRequest>),
}

/// Event emitted from the app-server to the in-process client.
///
/// event — it signals that the consumer fell behind and some events were dropped.
#[derive(Debug, Clone)]
pub enum InProcessServerEvent {
    /// Server request that requires client response/rejection.
    ServerRequest(ServerRequest),
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

    let runtime_handle = tokio::spawn(async move {
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

        let mut outbound_handle = tokio::spawn(async move {
            while let Some(envelope) = outgoing_rx.recv().await {
                route_outgoing_envelope(&outbound_connection_state, envelope).await;
            }
        });

        let processor_outgoing = Arc::clone(&outgoing_message_sender);
        let (processor_tx, mut processor_rx) = mpsc::channel::<ProcessorCommand>(channel_capacity);
        let mut processor_handle = tokio::spawn(async move {
            let processor = Arc::new(MessageProcessor::new(Arc::clone(&processor_outgoing)));
            let session = Arc::new(ConnectionSessionState::default());

            loop {
                tokio::select! {
                    command = processor_rx.recv() => {
                        match command {
                            Some(ProcessorCommand::Request(request)) => {
                                processor
                                    .process_client_request(
                                        *request,
                                        Arc::clone(&session),
                                        &outbound_initialized,
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
        });
        let mut pending_request_responses =
            HashMap::<RequestId, oneshot::Sender<ClientRequestResult>>::new();

        loop {
            tokio::select! {
                message = client_rx.recv() => {
                    todo!()
                }
                queued_message = writer_rx.recv() => {
                    todo!()
                }
            }
        }
        drop(writer_rx);
        drop(processor_tx);
        drop(outgoing_message_sender);
    });
    InProcessClientHandle {
        client_tx,
        event_rx,
        runtime_handle,
    }
}
