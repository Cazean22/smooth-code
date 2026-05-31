use std::{collections::HashSet, sync::Arc};

use app_server_protocol::{
    AskUserQuestionParams, AskUserQuestionResponse, ClientRequest, JsonRpcError,
    ServerRequestPayload, SetPlanModeResponse, ThreadListItem, ThreadListResponse,
    ThreadResumeResponse, ThreadStartResponse, TurnStartResponse,
};
use smooth_core::{AskUserClient, AskUserClientFactory, ThreadManagerState, ThreadSummary};
use smooth_protocol::ThreadId;
use tokio::sync::{Mutex, mpsc};
use tracing::Instrument;

use crate::{
    error::{AppServerError, AppServerResult},
    error_code::{INTERNAL_ERROR_CODE, SERVER_ERROR_CODE},
    in_process::InProcessServerEvent,
    outgoing_message::{OutgoingMessageSender, ThreadScopedOutgoingMessageSender},
};

pub(crate) struct CoreMessageProcessor {
    threads: ThreadManagerState,
    event_tx: mpsc::Sender<InProcessServerEvent>,
    subscribed_threads: Mutex<HashSet<ThreadId>>,
}

fn ask_user_client_factory(outgoing: Arc<OutgoingMessageSender>) -> AskUserClientFactory {
    AskUserClientFactory::new(move |thread_id| {
        let outgoing = ThreadScopedOutgoingMessageSender::new(Arc::clone(&outgoing), thread_id);
        let ask_outgoing = outgoing.clone();
        let abort_outgoing = outgoing;
        AskUserClient::new(
            move |params: AskUserQuestionParams| {
                let outgoing = ask_outgoing.clone();
                async move {
                    let (_, response_rx) = outgoing
                        .send_request(ServerRequestPayload::AskUserQuestion(params))
                        .await;
                    let value = match response_rx.await {
                        Ok(Ok(value)) => value,
                        Ok(Err(err)) => return Err(err),
                        Err(err) => {
                            return Err(JsonRpcError::message_only(
                                SERVER_ERROR_CODE,
                                "server_request_waiter_closed",
                                err.to_string(),
                            ));
                        }
                    };
                    serde_json::from_value::<AskUserQuestionResponse>(value).map_err(|err| {
                        JsonRpcError::message_only(
                            INTERNAL_ERROR_CODE,
                            "invalid_ask_user_question_response",
                            format!("invalid ask_user_question response: {err}"),
                        )
                    })
                }
            },
            move || {
                let outgoing = abort_outgoing.clone();
                async move {
                    outgoing.abort_pending_server_requests().await;
                }
            },
        )
    })
}

impl CoreMessageProcessor {
    pub async fn new(
        event_tx: mpsc::Sender<InProcessServerEvent>,
        outgoing: Arc<OutgoingMessageSender>,
    ) -> AppServerResult<Self> {
        Ok(Self {
            threads: ThreadManagerState::new(Some(ask_user_client_factory(outgoing)), None).await?,
            event_tx,
            subscribed_threads: Mutex::new(HashSet::new()),
        })
    }

    pub async fn process_request(
        &self,
        request: ClientRequest,
    ) -> Result<serde_json::Value, JsonRpcError> {
        self.process_request_inner(request)
            .await
            .map_err(JsonRpcError::from)
    }

    async fn process_request_inner(
        &self,
        request: ClientRequest,
    ) -> AppServerResult<serde_json::Value> {
        match request {
            ClientRequest::ThreadStart { .. } => {
                tracing::debug!("processing thread start request");
                let started = self.threads.start_thread().await?;
                self.ensure_thread_subscription(started.thread_id).await;
                self.threads
                    .emit_session_configured(started.thread_id)
                    .await?;
                Ok(serde_json::to_value(ThreadStartResponse {
                    thread_id: started.thread_id.to_string(),
                    rollout_path: started.rollout_path.display().to_string(),
                })?)
            }
            ClientRequest::TurnStart { params, .. } => {
                tracing::debug!(
                    thread_id = %params.thread_id,
                    input_len = params.input.len(),
                    "processing turn start request"
                );
                let thread_id = params
                    .thread_id
                    .parse::<ThreadId>()
                    .map_err(AppServerError::invalid_thread_id)?;
                self.ensure_thread_subscription(thread_id).await;
                let turn_id = self
                    .threads
                    .start_user_input(thread_id, params.input)
                    .await?;
                Ok(serde_json::to_value(TurnStartResponse {
                    thread_id: thread_id.to_string(),
                    turn_id,
                })?)
            }
            ClientRequest::ThreadResume { params, .. } => {
                tracing::debug!(
                    thread_id = %params.thread_id,
                    "processing thread resume request"
                );
                let thread_id = params
                    .thread_id
                    .parse::<ThreadId>()
                    .map_err(AppServerError::invalid_thread_id)?;
                let resumed = self.threads.resume_thread(thread_id).await?;
                self.ensure_thread_subscription(thread_id).await;
                Ok(serde_json::to_value(ThreadResumeResponse {
                    thread_id: resumed.thread_id.to_string(),
                    rollout_path: resumed.rollout_path.display().to_string(),
                    initial_messages: resumed.initial_messages,
                })?)
            }
            ClientRequest::ThreadList { params, .. } => {
                tracing::debug!(
                    cursor = ?params.cursor,
                    limit = params.limit,
                    "processing thread list request"
                );
                let threads = self.threads.list_threads().await?;
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
                Ok(serde_json::to_value(ThreadListResponse {
                    data: page,
                    next_cursor,
                })?)
            }
            ClientRequest::SetPlanMode { params, .. } => {
                tracing::debug!(
                    thread_id = %params.thread_id,
                    enabled = params.enabled,
                    "processing set plan mode request"
                );
                let thread_id = params
                    .thread_id
                    .parse::<ThreadId>()
                    .map_err(AppServerError::invalid_thread_id)?;
                let enabled = self
                    .threads
                    .set_plan_mode(thread_id, params.enabled)
                    .await?;
                Ok(serde_json::to_value(SetPlanModeResponse {
                    thread_id: thread_id.to_string(),
                    enabled,
                })?)
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
        if let Err(err) = tokio::task::Builder::new()
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
        {
            tracing::error!(
                thread_id = %thread_id,
                error = %err,
                "failed to spawn session event subscription task"
            );
        }
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

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, sync::Arc, sync::LazyLock};

    use app_server_protocol::{ClientRequest, RequestId, TurnStartParams};
    use tokio::sync::{Mutex as TokioMutex, mpsc};

    use super::CoreMessageProcessor;
    use crate::{
        error_code::INVALID_PARAMS_ERROR_CODE, in_process::InProcessServerEvent,
        outgoing_message::OutgoingMessageSender,
    };

    static CWD_LOCK: LazyLock<TokioMutex<()>> = LazyLock::new(|| TokioMutex::new(()));

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    struct CwdRestore(PathBuf);

    impl Drop for CwdRestore {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.0);
        }
    }

    #[tokio::test]
    async fn invalid_thread_id_request_preserves_structured_error_info() -> TestResult {
        let _cwd_guard = CWD_LOCK.lock().await;
        let workspace = tempfile::TempDir::new()?;
        let original_cwd = std::env::current_dir()?;
        std::env::set_current_dir(workspace.path())?;
        let _cwd_restore = CwdRestore(original_cwd);

        let (event_tx, _event_rx) = mpsc::channel::<InProcessServerEvent>(8);
        let outgoing = Arc::new(OutgoingMessageSender::new(event_tx.clone()));
        let processor = CoreMessageProcessor::new(event_tx, outgoing).await?;
        let request = ClientRequest::TurnStart {
            request_id: RequestId(1),
            params: TurnStartParams {
                thread_id: "not-a-thread-id".to_string(),
                input: "hello".to_string(),
            },
        };
        let Err(error) = processor.process_request(request).await else {
            panic!("invalid thread id should return an app-server error");
        };

        assert_eq!(error.code, INVALID_PARAMS_ERROR_CODE);
        let info = error.data.as_ref().ok_or("missing error data")?;
        assert_eq!(info.kind, "invalid_thread_id");
        assert_eq!(info.source.as_deref(), Some("app-server"));
        assert_eq!(error.message, info.message);
        Ok(())
    }
}
