use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use app_server_protocol::{JSONRPCErrorError, RequestId, ServerRequestPayload};
use smooth_protocol::ThreadId;
use tokio::sync::{Mutex, mpsc, oneshot};
use tracing::warn;

use crate::{
    ClientRequestResult,
    error_code::{INTERNAL_ERROR_CODE, OVERLOADED_ERROR_CODE, SERVER_ERROR_CODE},
    in_process::InProcessServerEvent,
};

pub(crate) struct OutgoingMessageSender {
    next_server_request_id: AtomicU64,
    event_tx: mpsc::Sender<InProcessServerEvent>,
    request_id_to_callback: Mutex<HashMap<RequestId, PendingCallbackEntry>>,
}

#[derive(Clone)]
pub(crate) struct ThreadScopedOutgoingMessageSender {
    outgoing: Arc<OutgoingMessageSender>,
    thread_id: ThreadId,
}

struct PendingCallbackEntry {
    callback: oneshot::Sender<ClientRequestResult>,
    thread_id: Option<ThreadId>,
}

impl ThreadScopedOutgoingMessageSender {
    pub(crate) fn new(outgoing: Arc<OutgoingMessageSender>, thread_id: ThreadId) -> Self {
        Self {
            outgoing,
            thread_id,
        }
    }

    pub(crate) async fn send_request(
        &self,
        payload: ServerRequestPayload,
    ) -> (RequestId, oneshot::Receiver<ClientRequestResult>) {
        self.outgoing
            .send_request_for_thread(payload, Some(self.thread_id))
            .await
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
    pub(crate) fn new(event_tx: mpsc::Sender<InProcessServerEvent>) -> Self {
        Self {
            next_server_request_id: AtomicU64::new(0),
            event_tx,
            request_id_to_callback: Mutex::new(HashMap::new()),
        }
    }

    #[cfg(test)]
    pub(crate) async fn send_request(
        &self,
        request: ServerRequestPayload,
    ) -> (RequestId, oneshot::Receiver<ClientRequestResult>) {
        self.send_request_for_thread(request, None).await
    }

    async fn send_request_for_thread(
        &self,
        request: ServerRequestPayload,
        thread_id: Option<ThreadId>,
    ) -> (RequestId, oneshot::Receiver<ClientRequestResult>) {
        let id = RequestId(self.next_server_request_id.fetch_add(1, Ordering::Relaxed) as usize);
        let request = request.request_with_id(id.clone());
        let (callback_tx, callback_rx) = oneshot::channel();
        {
            let mut request_id_to_callback = self.request_id_to_callback.lock().await;
            request_id_to_callback.insert(
                id.clone(),
                PendingCallbackEntry {
                    callback: callback_tx,
                    thread_id,
                },
            );
        }

        if let Err(send_error) = self
            .event_tx
            .try_send(InProcessServerEvent::ServerRequest(request))
        {
            let error = match send_error {
                mpsc::error::TrySendError::Full(_) => JSONRPCErrorError {
                    code: OVERLOADED_ERROR_CODE,
                    data: None,
                    message: "in-process server request queue is full".to_string(),
                },
                mpsc::error::TrySendError::Closed(_) => JSONRPCErrorError {
                    code: SERVER_ERROR_CODE,
                    data: None,
                    message: "in-process server request consumer is closed".to_string(),
                },
            };

            if let Some((request_id, entry)) = self.take_request_callback(&id).await {
                warn!("failed to send request {request_id:?} to client");
                let _ = entry.callback.send(Err(error));
            }
        }

        (id, callback_rx)
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
                    warn!("could not notify callback due to: {err:?}");
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
                    warn!("could not notify callback due to: {err:?}");
                }
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
    async fn send_request_emits_server_request_event() {
        let (event_tx, mut event_rx) = mpsc::channel(1);
        let outgoing = OutgoingMessageSender::new(event_tx);
        let thread_id = ThreadId::new();

        let (request_id, _response_rx) = outgoing
            .send_request(dynamic_tool_call(thread_id, "echo"))
            .await;

        let event = event_rx.recv().await.expect("expected outgoing event");
        match event {
            InProcessServerEvent::ServerRequest(
                app_server_protocol::ServerRequest::DynamicToolCall {
                    request_id: observed,
                    ..
                },
            ) => assert_eq!(observed, request_id),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn notify_client_error_forwards_error_to_waiter() {
        let (event_tx, _event_rx) = mpsc::channel(1);
        let outgoing = OutgoingMessageSender::new(event_tx);
        let thread_id = ThreadId::new();
        let (request_id, response_rx) = outgoing
            .send_request_for_thread(
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
    async fn send_request_fails_when_event_consumer_is_closed() {
        let (event_tx, event_rx) = mpsc::channel(1);
        drop(event_rx);
        let outgoing = OutgoingMessageSender::new(event_tx);
        let thread_id = ThreadId::new();

        let (_request_id, response_rx) = outgoing
            .send_request(dynamic_tool_call(thread_id, "echo"))
            .await;

        let response = timeout(Duration::from_secs(1), response_rx)
            .await
            .expect("response should resolve")
            .expect("response channel should stay open");
        assert!(matches!(
            response,
            Err(JSONRPCErrorError {
                code: SERVER_ERROR_CODE,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn cancel_requests_for_thread_cancels_all_thread_requests() {
        let (event_tx, _event_rx) = mpsc::channel(8);
        let outgoing = Arc::new(OutgoingMessageSender::new(event_tx));
        let thread_id = ThreadId::new();
        let other_thread_id = ThreadId::new();

        let (_, first_rx) = outgoing
            .send_request_for_thread(dynamic_tool_call(thread_id, "first"), Some(thread_id))
            .await;
        let (_, second_rx) = outgoing
            .send_request_for_thread(dynamic_tool_call(thread_id, "second"), Some(thread_id))
            .await;
        let (_, other_rx) = outgoing
            .send_request_for_thread(
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
        assert!(timeout(Duration::from_millis(50), other_rx).await.is_err());
    }
}
