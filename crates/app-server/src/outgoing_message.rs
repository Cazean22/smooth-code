use std::{collections::HashMap, sync::atomic::AtomicU64};

use app_server_protocol::{JSONRPCErrorError, RequestId, ServerRequest};
use serde::Serialize;
use smooth_protocol::ThreadId;
use tokio::sync::{Mutex, mpsc, oneshot};

use crate::ClientRequestResult;

#[derive(Debug)]
pub(crate) enum OutgoingEnvelope {
    ToConnection {
        message: OutgoingMessage,
        write_complete_tx: Option<oneshot::Sender<()>>,
    },
    Broadcast {
        message: OutgoingMessage,
    },
}

/// Outgoing message from the server to the client.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub(crate) enum OutgoingMessage {
    Request(ServerRequest),
    Response(OutgoingResponse),
    Error(OutgoingError),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct OutgoingResponse {
    pub id: RequestId,
    pub result: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct OutgoingError {
    pub id: RequestId,
    pub error: JSONRPCErrorError,
}

/// Sends messages to the client and manages request callbacks.
pub(crate) struct OutgoingMessageSender {
    next_server_request_id: AtomicU64,
    sender: mpsc::Sender<OutgoingEnvelope>,
    request_id_to_callback: Mutex<HashMap<RequestId, PendingCallbackEntry>>,
}

impl OutgoingMessageSender {
    pub(crate) fn new(sender: mpsc::Sender<OutgoingEnvelope>) -> Self {
        Self {
            next_server_request_id: AtomicU64::new(0),
            sender,
            request_id_to_callback: Mutex::new(HashMap::new()),
        }
    }
}

struct PendingCallbackEntry {
    callback: oneshot::Sender<ClientRequestResult>,
    thread_id: Option<ThreadId>,
    request: ServerRequest,
}

#[derive(Debug)]
pub(crate) struct QueuedOutgoingMessage {
    pub(crate) message: OutgoingMessage,
    pub(crate) write_complete_tx: Option<oneshot::Sender<()>>,
}

impl QueuedOutgoingMessage {
    pub(crate) fn new(message: OutgoingMessage) -> Self {
        Self {
            message,
            write_complete_tx: None,
        }
    }
}
