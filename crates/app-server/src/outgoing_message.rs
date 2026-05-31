use std::{
    collections::HashMap,
    sync::atomic::{AtomicU64, Ordering},
};

use app_server_protocol::{JsonRpcError, RequestId, ServerRequestPayload};
use smooth_protocol::{ErrorInfo, ThreadId};
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

struct PendingCallbackEntry {
    callback: oneshot::Sender<ClientRequestResult>,
    thread_id: Option<ThreadId>,
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
        self.send_request_inner(request, None).await
    }

    pub(crate) async fn send_request_for_thread(
        &self,
        thread_id: ThreadId,
        request: ServerRequestPayload,
    ) -> (RequestId, oneshot::Receiver<ClientRequestResult>) {
        self.send_request_inner(request, Some(thread_id)).await
    }

    pub(crate) async fn abort_pending_server_requests_for_thread(&self, thread_id: ThreadId) {
        self.cancel_requests_for_thread(
            thread_id,
            Some(JsonRpcError::new(
                INTERNAL_ERROR_CODE,
                ErrorInfo::new(
                    "turn_transition_pending_request",
                    "client request resolved because the turn state was changed",
                )
                .with_source("app-server"),
            )),
        )
        .await;
    }

    async fn send_request_inner(
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
                mpsc::error::TrySendError::Full(_) => JsonRpcError::new(
                    OVERLOADED_ERROR_CODE,
                    ErrorInfo::new(
                        "server_request_queue_full",
                        "in-process server request queue is full",
                    )
                    .with_source("app-server"),
                ),
                mpsc::error::TrySendError::Closed(_) => JsonRpcError::new(
                    SERVER_ERROR_CODE,
                    ErrorInfo::new(
                        "server_request_consumer_closed",
                        "in-process server request consumer is closed",
                    )
                    .with_source("app-server"),
                ),
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

    pub(crate) async fn notify_client_error(&self, id: RequestId, error: JsonRpcError) {
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

    pub(crate) async fn cancel_all_requests(&self, error: Option<JsonRpcError>) {
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
        error: Option<JsonRpcError>,
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

    use app_server_protocol::AskUserQuestionParams;
    use smooth_protocol::ErrorInfo;
    use smooth_protocol::ThreadId;
    use tokio::sync::mpsc;
    use tokio::time::timeout;

    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn ask_user_request(thread_id: ThreadId, call_id: &str) -> ServerRequestPayload {
        ServerRequestPayload::AskUserQuestion(AskUserQuestionParams {
            thread_id: thread_id.to_string(),
            turn_id: "turn-1".to_string(),
            call_id: call_id.to_string(),
            questions: Vec::new(),
        })
    }

    #[tokio::test]
    async fn send_request_emits_server_request_event() -> TestResult {
        let (event_tx, mut event_rx) = mpsc::channel(1);
        let outgoing = OutgoingMessageSender::new(event_tx);
        let thread_id = ThreadId::new();

        let (request_id, _response_rx) = outgoing
            .send_request(ask_user_request(thread_id, "call-1"))
            .await;

        let event = event_rx.recv().await.ok_or("expected outgoing event")?;
        match event {
            InProcessServerEvent::ServerRequest(
                app_server_protocol::ServerRequest::AskUserQuestion {
                    request_id: observed,
                    ..
                },
            ) => assert_eq!(observed, request_id),
            other => panic!("unexpected event: {other:?}"),
        }
        Ok(())
    }

    #[tokio::test]
    async fn notify_client_error_forwards_error_to_waiter() -> TestResult {
        let (event_tx, _event_rx) = mpsc::channel(1);
        let outgoing = OutgoingMessageSender::new(event_tx);
        let thread_id = ThreadId::new();
        let (request_id, response_rx) = outgoing
            .send_request_for_thread(thread_id, ask_user_request(thread_id, "call-1"))
            .await;
        let error = JsonRpcError::new(
            -32000,
            ErrorInfo::new("client_error", "client error").with_source("test"),
        );

        outgoing
            .notify_client_error(request_id, error.clone())
            .await;

        let response = timeout(Duration::from_secs(1), response_rx).await??;
        assert_eq!(response, Err(error));
        Ok(())
    }

    #[tokio::test]
    async fn send_request_fails_when_event_consumer_is_closed() -> TestResult {
        let (event_tx, event_rx) = mpsc::channel(1);
        drop(event_rx);
        let outgoing = OutgoingMessageSender::new(event_tx);
        let thread_id = ThreadId::new();

        let (_request_id, response_rx) = outgoing
            .send_request(ask_user_request(thread_id, "call-1"))
            .await;

        let response = timeout(Duration::from_secs(1), response_rx).await??;
        assert!(matches!(
            response,
            Err(JsonRpcError {
                code: SERVER_ERROR_CODE,
                ..
            })
        ));
        Ok(())
    }

    #[tokio::test]
    async fn cancel_requests_for_thread_cancels_all_thread_requests() -> TestResult {
        let (event_tx, _event_rx) = mpsc::channel(8);
        let outgoing = Arc::new(OutgoingMessageSender::new(event_tx));
        let thread_id = ThreadId::new();
        let other_thread_id = ThreadId::new();

        let (_, first_rx) = outgoing
            .send_request_for_thread(thread_id, ask_user_request(thread_id, "first"))
            .await;
        let (_, second_rx) = outgoing
            .send_request_for_thread(thread_id, ask_user_request(thread_id, "second"))
            .await;
        let (_, other_rx) = outgoing
            .send_request_for_thread(other_thread_id, ask_user_request(other_thread_id, "other"))
            .await;
        let error = JsonRpcError::new(
            -32002,
            ErrorInfo::new("cancelled", "cancelled").with_source("test"),
        );

        outgoing
            .cancel_requests_for_thread(thread_id, Some(error.clone()))
            .await;

        let first = timeout(Duration::from_secs(1), first_rx).await??;
        let second = timeout(Duration::from_secs(1), second_rx).await??;

        assert_eq!(first, Err(error.clone()));
        assert_eq!(second, Err(error));
        assert!(timeout(Duration::from_millis(50), other_rx).await.is_err());
        Ok(())
    }
}
