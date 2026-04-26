mod core_message_processor;
pub mod in_process;
mod message_processor;
mod outgoing_message;

use std::{
    collections::HashSet,
    sync::{Arc, atomic::AtomicBool},
};

use app_server_protocol::JSONRPCErrorError;
use tokio::sync::{RwLock, mpsc};
use tokio_util::sync::CancellationToken;

use crate::outgoing_message::{OutgoingEnvelope, OutgoingMessage, QueuedOutgoingMessage};
pub type ClientRequestResult = std::result::Result<serde_json::Value, JSONRPCErrorError>;

pub(crate) struct OutboundConnectionState {
    pub(crate) initialized: Arc<AtomicBool>,
    pub(crate) opted_out_notification_methods: Arc<RwLock<HashSet<String>>>,
    pub(crate) writer: mpsc::Sender<QueuedOutgoingMessage>,
    pub(crate) disconnect_sender: Option<CancellationToken>,
}

impl OutboundConnectionState {
    pub(crate) fn new(
        writer: mpsc::Sender<QueuedOutgoingMessage>,
        initialized: Arc<AtomicBool>,
        opted_out_notification_methods: Arc<RwLock<HashSet<String>>>,
        disconnect_sender: Option<CancellationToken>,
    ) -> Self {
        Self {
            initialized,
            opted_out_notification_methods,
            writer,
            disconnect_sender,
        }
    }

    pub(crate) fn can_disconnect(&self) -> bool {
        self.disconnect_sender.is_some()
    }

    pub(crate) fn request_disconnect(&self) {
        if let Some(disconnect_sender) = &self.disconnect_sender {
            disconnect_sender.cancel();
        }
    }
}

async fn send_message_to_connection(
    connection_state: &OutboundConnectionState,
    message: OutgoingMessage,
    write_complete_tx: Option<tokio::sync::oneshot::Sender<()>>,
) -> bool {
    let writer = connection_state.writer.clone();
    let queued_message = QueuedOutgoingMessage {
        message,
        write_complete_tx,
    };
    if connection_state.can_disconnect() {
        match writer.try_send(queued_message) {
            Ok(()) => false,
            Err(_) => {
                connection_state.request_disconnect();
                true
            }
        }
    } else if writer.send(queued_message).await.is_err() {
        connection_state.request_disconnect();
        true
    } else {
        false
    }
}

pub(crate) async fn route_outgoing_envelope(
    connection_state: &OutboundConnectionState,
    envelope: OutgoingEnvelope,
) {
    match envelope {
        OutgoingEnvelope::ToConnection {
            message,
            write_complete_tx,
        } => {
            let _ = send_message_to_connection(connection_state, message, write_complete_tx).await;
        }
        OutgoingEnvelope::Broadcast { message } => {
            let _ = send_message_to_connection(connection_state, message, None).await;
        }
    }
}
