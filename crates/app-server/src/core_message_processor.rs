use std::{collections::HashSet, sync::Arc};

use app_server_protocol::{
    AskUserQuestionParams, AskUserQuestionResponse, ClientRequest, JSONRPCErrorError,
    ServerRequestPayload, SetPlanModeResponse, ThreadListItem, ThreadListResponse,
    ThreadResumeResponse, ThreadStartResponse, TurnStartResponse,
};
use futures_util::future::BoxFuture;
use smooth_core::{AskUserClient, AskUserClientFactory, ThreadManagerState, ThreadSummary};
use smooth_protocol::ThreadId;
use tokio::sync::{Mutex, mpsc};
use tracing::Instrument;

use crate::{
    error_code::{INTERNAL_ERROR_CODE, INVALID_PARAMS_ERROR_CODE, SERVER_ERROR_CODE},
    in_process::InProcessServerEvent,
    outgoing_message::{OutgoingMessageSender, ThreadScopedOutgoingMessageSender},
};

pub(crate) struct CoreMessageProcessor {
    threads: ThreadManagerState,
    event_tx: mpsc::Sender<InProcessServerEvent>,
    subscribed_threads: Mutex<HashSet<ThreadId>>,
}

struct InProcessAskUserClientFactory {
    outgoing: Arc<OutgoingMessageSender>,
}

struct InProcessAskUserClient {
    outgoing: ThreadScopedOutgoingMessageSender,
}

impl AskUserClientFactory for InProcessAskUserClientFactory {
    fn build(&self, thread_id: ThreadId) -> Arc<dyn AskUserClient> {
        Arc::new(InProcessAskUserClient {
            outgoing: ThreadScopedOutgoingMessageSender::new(Arc::clone(&self.outgoing), thread_id),
        })
    }
}

impl AskUserClient for InProcessAskUserClient {
    fn ask(
        &self,
        params: AskUserQuestionParams,
    ) -> BoxFuture<'static, Result<AskUserQuestionResponse, JSONRPCErrorError>> {
        let outgoing = self.outgoing.clone();
        Box::pin(async move {
            let (_, response_rx) = outgoing
                .send_request(ServerRequestPayload::AskUserQuestion(params))
                .await;
            let value = match response_rx.await {
                Ok(Ok(value)) => value,
                Ok(Err(err)) => return Err(err),
                Err(err) => {
                    return Err(JSONRPCErrorError {
                        code: SERVER_ERROR_CODE,
                        data: None,
                        message: err.to_string(),
                    });
                }
            };
            serde_json::from_value::<AskUserQuestionResponse>(value).map_err(|err| {
                JSONRPCErrorError {
                    code: INTERNAL_ERROR_CODE,
                    data: None,
                    message: format!("invalid ask_user_question response: {err}"),
                }
            })
        })
    }

    fn abort_pending_server_requests(&self) -> BoxFuture<'static, ()> {
        let outgoing = self.outgoing.clone();
        Box::pin(async move {
            outgoing.abort_pending_server_requests().await;
        })
    }
}

impl CoreMessageProcessor {
    pub async fn new(
        event_tx: mpsc::Sender<InProcessServerEvent>,
        outgoing: Arc<OutgoingMessageSender>,
    ) -> anyhow::Result<Self> {
        let ask_user_client_factory: Arc<dyn AskUserClientFactory> =
            Arc::new(InProcessAskUserClientFactory { outgoing });
        Ok(Self {
            threads: ThreadManagerState::new(Some(ask_user_client_factory), None).await?,
            event_tx,
            subscribed_threads: Mutex::new(HashSet::new()),
        })
    }

    pub async fn process_request(
        &self,
        request: ClientRequest,
    ) -> Result<serde_json::Value, JSONRPCErrorError> {
        match request {
            ClientRequest::ThreadStart { .. } => {
                tracing::debug!("processing thread start request");
                let started = self.threads.start_thread().await.map_err(internal_error)?;
                self.ensure_thread_subscription(started.thread_id).await;
                self.threads
                    .emit_session_configured(started.thread_id)
                    .await
                    .map_err(internal_error)?;
                serde_json::to_value(ThreadStartResponse {
                    thread_id: started.thread_id.to_string(),
                    rollout_path: started.rollout_path.display().to_string(),
                })
                .map_err(internal_serde_error)
            }
            ClientRequest::TurnStart { params, .. } => {
                tracing::debug!(
                    thread_id = %params.thread_id,
                    input_len = params.input.len(),
                    "processing turn start request"
                );
                let thread_id =
                    params
                        .thread_id
                        .parse::<ThreadId>()
                        .map_err(|err| JSONRPCErrorError {
                            code: INVALID_PARAMS_ERROR_CODE,
                            data: None,
                            message: format!("invalid thread id: {err}"),
                        })?;
                self.ensure_thread_subscription(thread_id).await;
                let turn_id = self
                    .threads
                    .start_user_input(thread_id, params.input)
                    .await
                    .map_err(internal_error)?;
                serde_json::to_value(TurnStartResponse {
                    thread_id: thread_id.to_string(),
                    turn_id,
                })
                .map_err(internal_serde_error)
            }
            ClientRequest::ThreadResume { params, .. } => {
                tracing::debug!(
                    thread_id = %params.thread_id,
                    "processing thread resume request"
                );
                let thread_id =
                    params
                        .thread_id
                        .parse::<ThreadId>()
                        .map_err(|err| JSONRPCErrorError {
                            code: INVALID_PARAMS_ERROR_CODE,
                            data: None,
                            message: format!("invalid thread id: {err}"),
                        })?;
                let resumed = self
                    .threads
                    .resume_thread(thread_id)
                    .await
                    .map_err(internal_error)?;
                self.ensure_thread_subscription(thread_id).await;
                serde_json::to_value(ThreadResumeResponse {
                    thread_id: resumed.thread_id.to_string(),
                    rollout_path: resumed.rollout_path.display().to_string(),
                    initial_messages: resumed.initial_messages,
                })
                .map_err(internal_serde_error)
            }
            ClientRequest::ThreadList { params, .. } => {
                tracing::debug!(
                    cursor = ?params.cursor,
                    limit = params.limit,
                    "processing thread list request"
                );
                let threads = self.threads.list_threads().await.map_err(internal_error)?;
                let offset = params
                    .cursor
                    .as_deref()
                    .and_then(|cursor| cursor.parse::<usize>().ok())
                    .unwrap_or(0);
                let limit = params.limit.unwrap_or(20) as usize;
                let page = threads
                    .into_iter()
                    .skip(offset)
                    .take(limit)
                    .map(map_thread_summary)
                    .collect::<Vec<_>>();
                let next_cursor = (page.len() == limit).then(|| (offset + limit).to_string());
                serde_json::to_value(ThreadListResponse {
                    data: page,
                    next_cursor,
                })
                .map_err(internal_serde_error)
            }
            ClientRequest::SetPlanMode { params, .. } => {
                tracing::debug!(
                    thread_id = %params.thread_id,
                    enabled = params.enabled,
                    "processing set plan mode request"
                );
                let thread_id =
                    params
                        .thread_id
                        .parse::<ThreadId>()
                        .map_err(|err| JSONRPCErrorError {
                            code: INVALID_PARAMS_ERROR_CODE,
                            data: None,
                            message: format!("invalid thread id: {err}"),
                        })?;
                let enabled = self
                    .threads
                    .set_plan_mode(thread_id, params.enabled)
                    .await
                    .map_err(internal_error)?;
                serde_json::to_value(SetPlanModeResponse {
                    thread_id: thread_id.to_string(),
                    enabled,
                })
                .map_err(internal_serde_error)
            }
        }
    }

    async fn ensure_thread_subscription(&self, thread_id: ThreadId) {
        {
            let mut subscribed = self.subscribed_threads.lock().await;
            if !subscribed.insert(thread_id) {
                return;
            }
        }

        let Ok(mut rx) = self.threads.subscribe(thread_id).await else {
            return;
        };
        let event_tx = self.event_tx.clone();
        let subscription_span = tracing::info_span!(
            "app_server.session_event_subscription",
            thread_id = %thread_id,
        );
        tokio::task::Builder::new()
            .name("app_server.session_subscription")
            .spawn(
                async move {
                    loop {
                        match rx.recv().await {
                            Ok(event) => {
                                if event_tx
                                    .send(InProcessServerEvent::SessionEvent { thread_id, event })
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                                tracing::warn!(skipped, "session event subscription lagged");
                                continue;
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    }
                }
                .instrument(subscription_span),
            )
            .expect("failed to spawn session event subscription task");
    }
}

fn internal_error(err: anyhow::Error) -> JSONRPCErrorError {
    JSONRPCErrorError {
        code: SERVER_ERROR_CODE,
        data: None,
        message: err.to_string(),
    }
}

fn internal_serde_error(err: serde_json::Error) -> JSONRPCErrorError {
    JSONRPCErrorError {
        code: INTERNAL_ERROR_CODE,
        data: None,
        message: err.to_string(),
    }
}

fn map_thread_summary(summary: ThreadSummary) -> ThreadListItem {
    ThreadListItem {
        thread_id: summary.thread_id.to_string(),
        rollout_path: summary.rollout_path.display().to_string(),
        created_at: summary.created_at,
        updated_at: summary.updated_at,
        last_user_message: summary.last_user_message,
        last_assistant_message: summary.last_assistant_message,
    }
}
