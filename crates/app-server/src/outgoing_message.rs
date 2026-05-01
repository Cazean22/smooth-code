use std::{
    collections::HashMap,
    fmt,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use app_server_protocol::{JSONRPCErrorError, RequestId, ServerRequest, ServerRequestPayload};
use serde::Serialize;
use smooth_protocol::ThreadId;
use tokio::sync::{Mutex, mpsc, oneshot};
use tracing::warn;

use crate::ClientRequestResult;

const INTERNAL_ERROR_CODE: i64 = -32603;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ConnectionId(pub(crate) u64);

impl fmt::Display for ConnectionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ConnectionRequestId {
    pub(crate) connection_id: ConnectionId,
    pub(crate) request_id: RequestId,
}

#[derive(Debug)]
pub(crate) enum OutgoingEnvelope {
    ToConnection {
        connection_id: ConnectionId,
        message: OutgoingMessage,
        write_complete_tx: Option<oneshot::Sender<()>>,
    },
    Broadcast {
        message: OutgoingMessage,
    },
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

pub(crate) struct OutgoingMessageSender {
    next_server_request_id: AtomicU64,
    sender: mpsc::Sender<OutgoingEnvelope>,
    request_id_to_callback: Mutex<HashMap<RequestId, PendingCallbackEntry>>,
}

#[derive(Clone)]
pub(crate) struct ThreadScopedOutgoingMessageSender {
    outgoing: Arc<OutgoingMessageSender>,
    connection_ids: Arc<Vec<ConnectionId>>,
    thread_id: ThreadId,
}

struct PendingCallbackEntry {
    callback: oneshot::Sender<ClientRequestResult>,
    thread_id: Option<ThreadId>,
    request: ServerRequest,
}

impl ThreadScopedOutgoingMessageSender {
    pub(crate) fn new(
        outgoing: Arc<OutgoingMessageSender>,
        connection_ids: Vec<ConnectionId>,
        thread_id: ThreadId,
    ) -> Self {
        Self {
            outgoing,
            connection_ids: Arc::new(connection_ids),
            thread_id,
        }
    }

    pub(crate) async fn send_request(
        &self,
        payload: ServerRequestPayload,
    ) -> (RequestId, oneshot::Receiver<ClientRequestResult>) {
        self.outgoing
            .send_request_to_connections(
                Some(self.connection_ids.as_slice()),
                payload,
                Some(self.thread_id),
            )
            .await
    }

    pub(crate) async fn send_response<T: Serialize>(
        &self,
        request_id: ConnectionRequestId,
        response: T,
    ) {
        self.outgoing.send_response(request_id, response).await;
    }

    pub(crate) async fn send_error(
        &self,
        request_id: ConnectionRequestId,
        error: JSONRPCErrorError,
    ) {
        self.outgoing.send_error(request_id, error).await;
    }

    pub(crate) async fn abort_pending_server_requests(&self) {
        self.outgoing
            .cancel_requests_for_thread(
                self.thread_id,
                Some(JSONRPCErrorError {
                    code: INTERNAL_ERROR_CODE,
                    message: "client request resolved because the turn state was changed"
                        .to_string(),
                    data: Some(serde_json::json!({ "reason": "turn_transition_pending_request" })),
                }),
            )
            .await;
    }
}

impl OutgoingMessageSender {
    pub(crate) fn new(sender: mpsc::Sender<OutgoingEnvelope>) -> Self {
        Self {
            next_server_request_id: AtomicU64::new(0),
            sender,
            request_id_to_callback: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) async fn send_request(
        &self,
        request: ServerRequestPayload,
    ) -> (RequestId, oneshot::Receiver<ClientRequestResult>) {
        self.send_request_to_connections(
            /* connection_ids */ None, request, /* thread_id */ None,
        )
        .await
    }

    pub(crate) fn next_request_id(&self) -> RequestId {
        RequestId(self.next_server_request_id.fetch_add(1, Ordering::Relaxed) as usize)
    }

    pub(crate) async fn send_request_to_connections(
        &self,
        connection_ids: Option<&[ConnectionId]>,
        request: ServerRequestPayload,
        thread_id: Option<ThreadId>,
    ) -> (RequestId, oneshot::Receiver<ClientRequestResult>) {
        let id = self.next_request_id();
        let request = request.request_with_id(id.clone());
        let (callback_tx, callback_rx) = oneshot::channel();
        {
            let mut request_id_to_callback = self.request_id_to_callback.lock().await;
            request_id_to_callback.insert(
                id.clone(),
                PendingCallbackEntry {
                    callback: callback_tx,
                    thread_id,
                    request: request.clone(),
                },
            );
        }

        let outgoing_message = OutgoingMessage::Request(request);
        let send_result = match connection_ids {
            None => {
                self.sender
                    .send(OutgoingEnvelope::Broadcast {
                        message: outgoing_message,
                    })
                    .await
            }
            Some(connection_ids) => {
                let mut send_error = None;
                for connection_id in connection_ids {
                    if let Err(err) = self
                        .sender
                        .send(OutgoingEnvelope::ToConnection {
                            connection_id: *connection_id,
                            message: outgoing_message.clone(),
                            write_complete_tx: None,
                        })
                        .await
                    {
                        send_error = Some(err);
                        break;
                    }
                }
                match send_error {
                    Some(err) => Err(err),
                    None => Ok(()),
                }
            }
        };

        if let Err(err) = send_result {
            warn!("failed to send request {id:?} to client: {err:?}");
            let mut request_id_to_callback = self.request_id_to_callback.lock().await;
            request_id_to_callback.remove(&id);
        }

        (id, callback_rx)
    }

    pub(crate) async fn send_response<T: Serialize>(
        &self,
        request_id: ConnectionRequestId,
        response: T,
    ) {
        match serde_json::to_value(response) {
            Ok(result) => {
                self.send_outgoing_message_to_connection(
                    request_id.connection_id,
                    OutgoingMessage::Response(OutgoingResponse {
                        id: request_id.request_id,
                        result,
                    }),
                    "response",
                )
                .await;
            }
            Err(err) => {
                self.send_error(
                    request_id,
                    JSONRPCErrorError {
                        code: INTERNAL_ERROR_CODE,
                        message: format!("failed to serialize response: {err}"),
                        data: None,
                    },
                )
                .await;
            }
        }
    }

    pub(crate) async fn send_error(
        &self,
        request_id: ConnectionRequestId,
        error: JSONRPCErrorError,
    ) {
        self.send_outgoing_message_to_connection(
            request_id.connection_id,
            OutgoingMessage::Error(OutgoingError {
                id: request_id.request_id,
                error,
            }),
            "error",
        )
        .await;
    }

    pub(crate) async fn notify_client_response(&self, id: RequestId, result: serde_json::Value) {
        match self.take_request_callback(&id).await {
            Some((id, entry)) => {
                if let Err(err) = entry.callback.send(Ok(result)) {
                    warn!("could not notify callback for {id:?} due to: {err:?}");
                }
            }
            None => {
                warn!("could not find callback for {id:?}");
            }
        }
    }

    pub(crate) async fn notify_client_error(&self, id: RequestId, error: JSONRPCErrorError) {
        match self.take_request_callback(&id).await {
            Some((id, entry)) => {
                warn!("client responded with error for {id:?}: {error:?}");
                if let Err(err) = entry.callback.send(Err(error)) {
                    warn!("could not notify callback for {id:?} due to: {err:?}");
                }
            }
            None => {
                warn!("could not find callback for {id:?}");
            }
        }
    }

    pub(crate) async fn cancel_request(&self, id: &RequestId) -> bool {
        self.take_request_callback(id).await.is_some()
    }

    pub(crate) async fn cancel_all_requests(&self, error: Option<JSONRPCErrorError>) {
        let entries = {
            let mut request_id_to_callback = self.request_id_to_callback.lock().await;
            request_id_to_callback
                .drain()
                .map(|(_, entry)| entry)
                .collect::<Vec<_>>()
        };

        if let Some(error) = error {
            for entry in entries {
                if let Err(err) = entry.callback.send(Err(error.clone())) {
                    warn!(
                        "could not notify callback for {:?} due to: {err:?}",
                        entry.request.id()
                    );
                }
            }
        }
    }

    pub(crate) async fn cancel_requests_for_thread(
        &self,
        thread_id: ThreadId,
        error: Option<JSONRPCErrorError>,
    ) {
        let entries = {
            let mut request_id_to_callback = self.request_id_to_callback.lock().await;
            let request_ids = request_id_to_callback
                .iter()
                .filter_map(|(request_id, entry)| {
                    (entry.thread_id == Some(thread_id)).then_some(request_id.clone())
                })
                .collect::<Vec<_>>();

            let mut entries = Vec::with_capacity(request_ids.len());
            for request_id in request_ids {
                if let Some(entry) = request_id_to_callback.remove(&request_id) {
                    entries.push(entry);
                }
            }
            entries
        };

        if let Some(error) = error {
            for entry in entries {
                if let Err(err) = entry.callback.send(Err(error.clone())) {
                    warn!(
                        "could not notify callback for {:?} due to: {err:?}",
                        entry.request.id()
                    );
                }
            }
        }
    }

    pub(crate) async fn pending_requests_for_thread(
        &self,
        thread_id: ThreadId,
    ) -> Vec<ServerRequest> {
        let request_id_to_callback = self.request_id_to_callback.lock().await;
        let mut requests = request_id_to_callback
            .values()
            .filter_map(|entry| {
                (entry.thread_id == Some(thread_id)).then_some(entry.request.clone())
            })
            .collect::<Vec<_>>();
        requests.sort_by(|left, right| left.id().cmp(right.id()));
        requests
    }

    pub(crate) async fn replay_requests_to_connection_for_thread(
        &self,
        connection_id: ConnectionId,
        thread_id: ThreadId,
    ) {
        let requests = self.pending_requests_for_thread(thread_id).await;
        for request in requests {
            if let Err(err) = self
                .sender
                .send(OutgoingEnvelope::ToConnection {
                    connection_id,
                    message: OutgoingMessage::Request(request),
                    write_complete_tx: None,
                })
                .await
            {
                warn!("failed to resend request to client: {err:?}");
            }
        }
    }

    async fn take_request_callback(
        &self,
        id: &RequestId,
    ) -> Option<(RequestId, PendingCallbackEntry)> {
        let mut request_id_to_callback = self.request_id_to_callback.lock().await;
        request_id_to_callback.remove_entry(id)
    }

    async fn send_outgoing_message_to_connection(
        &self,
        connection_id: ConnectionId,
        message: OutgoingMessage,
        message_kind: &'static str,
    ) {
        if let Err(err) = self
            .sender
            .send(OutgoingEnvelope::ToConnection {
                connection_id,
                message,
                write_complete_tx: None,
            })
            .await
        {
            warn!("failed to send {message_kind} to client: {err:?}");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use app_server_protocol::DynamicToolCallParams;
    use serde_json::json;
    use smooth_protocol::ThreadId;
    use tokio::sync::mpsc;
    use tokio::time::timeout;

    use super::*;

    fn dynamic_tool_call(thread_id: ThreadId, tool: &str) -> ServerRequestPayload {
        ServerRequestPayload::DynamicToolCall(DynamicToolCallParams {
            thread_id: thread_id.to_string(),
            turn_id: "turn-1".to_string(),
            call_id: "call-1".to_string(),
            tool: tool.to_string(),
            arguments: json!({ "tool": tool }),
        })
    }

    #[tokio::test]
    async fn send_response_routes_to_target_connection() {
        let (tx, mut rx) = mpsc::channel(1);
        let outgoing = OutgoingMessageSender::new(tx);
        let request_id = ConnectionRequestId {
            connection_id: ConnectionId(7),
            request_id: RequestId(42),
        };

        outgoing
            .send_response(request_id.clone(), json!({ "ok": true }))
            .await;

        let envelope = rx.recv().await.expect("expected outgoing envelope");
        assert!(matches!(
            envelope,
            OutgoingEnvelope::ToConnection {
                connection_id: ConnectionId(7),
                message: OutgoingMessage::Response(OutgoingResponse {
                    id: RequestId(42),
                    result
                }),
                write_complete_tx: None,
            } if result == json!({ "ok": true })
        ));
    }

    #[tokio::test]
    async fn send_error_routes_to_target_connection() {
        let (tx, mut rx) = mpsc::channel(1);
        let outgoing = OutgoingMessageSender::new(tx);
        let request_id = ConnectionRequestId {
            connection_id: ConnectionId(9),
            request_id: RequestId(77),
        };
        let error = JSONRPCErrorError {
            code: -32001,
            data: Some(json!({ "kind": "stub" })),
            message: "failed".to_string(),
        };

        outgoing.send_error(request_id.clone(), error.clone()).await;

        let envelope = rx.recv().await.expect("expected outgoing envelope");
        assert!(matches!(
            envelope,
            OutgoingEnvelope::ToConnection {
                connection_id: ConnectionId(9),
                message: OutgoingMessage::Error(OutgoingError {
                    id: RequestId(77),
                    error: queued_error,
                }),
                write_complete_tx: None,
            } if queued_error == error
        ));
    }

    #[tokio::test]
    async fn notify_client_error_forwards_error_to_waiter() {
        let (tx, _rx) = mpsc::channel(1);
        let outgoing = OutgoingMessageSender::new(tx);
        let thread_id = ThreadId::new();
        let (request_id, response_rx) = outgoing
            .send_request_to_connections(
                Some(&[ConnectionId(0)]),
                dynamic_tool_call(thread_id, "dynamic_echo"),
                Some(thread_id),
            )
            .await;
        let error = JSONRPCErrorError {
            code: -32000,
            data: None,
            message: "client error".to_string(),
        };

        outgoing
            .notify_client_error(request_id, error.clone())
            .await;

        let response = timeout(Duration::from_secs(1), response_rx)
            .await
            .expect("response should resolve")
            .expect("response channel should stay open");
        assert_eq!(response, Err(error));
    }

    #[tokio::test]
    async fn pending_requests_for_thread_returns_thread_requests_in_request_id_order() {
        let (tx, _rx) = mpsc::channel(8);
        let outgoing = OutgoingMessageSender::new(tx);
        let thread_id = ThreadId::new();
        let other_thread_id = ThreadId::new();

        let _ = outgoing
            .send_request_to_connections(
                Some(&[ConnectionId(0)]),
                dynamic_tool_call(thread_id, "first"),
                Some(thread_id),
            )
            .await;
        let _ = outgoing
            .send_request_to_connections(
                Some(&[ConnectionId(0)]),
                dynamic_tool_call(other_thread_id, "other"),
                Some(other_thread_id),
            )
            .await;
        let _ = outgoing
            .send_request_to_connections(
                Some(&[ConnectionId(0)]),
                dynamic_tool_call(thread_id, "second"),
                Some(thread_id),
            )
            .await;

        let pending = outgoing.pending_requests_for_thread(thread_id).await;
        let ids = pending
            .iter()
            .map(|request| request.id().clone())
            .collect::<Vec<_>>();

        assert_eq!(ids, vec![RequestId(0), RequestId(2)]);
    }

    #[tokio::test]
    async fn cancel_requests_for_thread_cancels_all_thread_requests() {
        let (tx, _rx) = mpsc::channel(8);
        let outgoing = Arc::new(OutgoingMessageSender::new(tx));
        let thread_id = ThreadId::new();
        let other_thread_id = ThreadId::new();

        let (_, first_rx) = outgoing
            .send_request_to_connections(
                Some(&[ConnectionId(0)]),
                dynamic_tool_call(thread_id, "first"),
                Some(thread_id),
            )
            .await;
        let (_, second_rx) = outgoing
            .send_request_to_connections(
                Some(&[ConnectionId(0)]),
                dynamic_tool_call(thread_id, "second"),
                Some(thread_id),
            )
            .await;
        let (_, other_rx) = outgoing
            .send_request_to_connections(
                Some(&[ConnectionId(0)]),
                dynamic_tool_call(other_thread_id, "other"),
                Some(other_thread_id),
            )
            .await;
        let error = JSONRPCErrorError {
            code: -32002,
            data: None,
            message: "cancelled".to_string(),
        };

        outgoing
            .cancel_requests_for_thread(thread_id, Some(error.clone()))
            .await;

        let first = timeout(Duration::from_secs(1), first_rx)
            .await
            .expect("first response should resolve")
            .expect("first channel should stay open");
        let second = timeout(Duration::from_secs(1), second_rx)
            .await
            .expect("second response should resolve")
            .expect("second channel should stay open");

        assert_eq!(first, Err(error.clone()));
        assert_eq!(second, Err(error));
        assert!(
            outgoing
                .pending_requests_for_thread(thread_id)
                .await
                .is_empty()
        );
        assert_eq!(
            outgoing
                .pending_requests_for_thread(other_thread_id)
                .await
                .len(),
            1
        );
        assert!(timeout(Duration::from_millis(50), other_rx).await.is_err());
    }
}
