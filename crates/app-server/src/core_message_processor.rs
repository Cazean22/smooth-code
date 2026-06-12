use std::{collections::HashMap, sync::Arc};

use app_server_protocol::{
    AskUserQuestionParams, AskUserQuestionResponse, ClientRequest, JsonRpcError,
    RequestPlanApprovalParams, RequestPlanApprovalResponse, ServerRequestPayload,
    SetPlanModeResponse, ShutdownResponse, ThreadListItem, ThreadListResponse,
    ThreadPreviewResponse, ThreadResumeResponse, ThreadStartResponse, ThreadUnwatchResponse,
    TurnCancelResponse, TurnStartResponse,
};
use smooth_core::{AskUserClient, CoreError, ThreadManagerState, ThreadSummary};
use smooth_protocol::ThreadId;
use tokio::sync::{Mutex, mpsc};
use tracing::Instrument;

use crate::{
    error::{AppServerError, AppServerResult},
    error_code::{INTERNAL_ERROR_CODE, INVALID_PARAMS_ERROR_CODE, SERVER_ERROR_CODE},
    in_process::InProcessServerEvent,
    outgoing_message::OutgoingMessageSender,
};

/// A live event-forwarding task for one thread, with the owners that keep it
/// alive: the root session (never released by preview cleanup) and/or a
/// refcount of preview watchers.
struct ThreadSubscription {
    abort: tokio::task::AbortHandle,
    root: bool,
    preview_watchers: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubscriptionOwner {
    Root,
    Preview,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SubscriptionOutcome {
    Subscribed,
    AlreadySubscribed,
    /// The thread has no live event channel (`CoreError::UnknownThread`).
    /// Expected for previews of completed threads; an error for roots.
    NotLive,
}

pub(crate) struct CoreMessageProcessor {
    threads: ThreadManagerState,
    event_tx: mpsc::Sender<InProcessServerEvent>,
    subscribed_threads: Mutex<HashMap<ThreadId, ThreadSubscription>>,
}

fn ask_user_client(outgoing: Arc<OutgoingMessageSender>) -> AskUserClient {
    let ask_source = Arc::clone(&outgoing);
    let approval_source = Arc::clone(&outgoing);
    AskUserClient::new(
        move |params: AskUserQuestionParams| {
            let outgoing = Arc::clone(&ask_source);
            async move {
                let thread_id = params.thread_id.parse::<ThreadId>().map_err(|err| {
                    JsonRpcError::message_only(
                        INVALID_PARAMS_ERROR_CODE,
                        "invalid_thread_id",
                        format!("invalid ask_user_question thread id: {err}"),
                    )
                })?;
                let (_, response_rx) = outgoing
                    .send_request_for_thread(
                        thread_id,
                        ServerRequestPayload::AskUserQuestion(params),
                    )
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
        move |thread_id| {
            let outgoing = Arc::clone(&outgoing);
            async move {
                outgoing
                    .abort_pending_server_requests_for_thread(thread_id)
                    .await;
            }
        },
    )
    .with_plan_approval(move |params: RequestPlanApprovalParams| {
        let outgoing = Arc::clone(&approval_source);
        async move {
            let thread_id = params.thread_id.parse::<ThreadId>().map_err(|err| {
                JsonRpcError::message_only(
                    INVALID_PARAMS_ERROR_CODE,
                    "invalid_thread_id",
                    format!("invalid request_plan_approval thread id: {err}"),
                )
            })?;
            let (_, response_rx) = outgoing
                .send_request_for_thread(
                    thread_id,
                    ServerRequestPayload::RequestPlanApproval(params),
                )
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
            serde_json::from_value::<RequestPlanApprovalResponse>(value).map_err(|err| {
                JsonRpcError::message_only(
                    INTERNAL_ERROR_CODE,
                    "invalid_request_plan_approval_response",
                    format!("invalid request_plan_approval response: {err}"),
                )
            })
        }
    })
}

impl CoreMessageProcessor {
    pub async fn new(
        event_tx: mpsc::Sender<InProcessServerEvent>,
        outgoing: Arc<OutgoingMessageSender>,
    ) -> AppServerResult<Self> {
        Ok(Self {
            threads: ThreadManagerState::new(Some(ask_user_client(outgoing)), None).await?,
            event_tx,
            subscribed_threads: Mutex::new(HashMap::new()),
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
            ClientRequest::ThreadStart { params, .. } => {
                tracing::debug!("processing thread start request");
                let started = self
                    .threads
                    .start_thread_with_project_instructions(params.project_instructions)
                    .await?;
                self.ensure_root_subscription(started.thread_id).await?;
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
                self.ensure_root_subscription(thread_id).await?;
                let turn_id = self
                    .threads
                    .start_user_input(thread_id, params.input)
                    .await?;
                Ok(serde_json::to_value(TurnStartResponse {
                    thread_id: thread_id.to_string(),
                    turn_id,
                })?)
            }
            ClientRequest::TurnCancel { params, .. } => {
                tracing::debug!(
                    thread_id = %params.thread_id,
                    "processing turn cancel request"
                );
                let thread_id = params
                    .thread_id
                    .parse::<ThreadId>()
                    .map_err(AppServerError::invalid_thread_id)?;
                let cancelled_thread_ids = self
                    .threads
                    .cancel_turn_subtree(thread_id)
                    .await?
                    .into_iter()
                    .map(|thread_id| thread_id.to_string())
                    .collect();
                Ok(serde_json::to_value(TurnCancelResponse {
                    thread_id: thread_id.to_string(),
                    cancelled_thread_ids,
                })?)
            }
            ClientRequest::Shutdown { .. } => {
                tracing::debug!("processing shutdown request");
                self.threads.shutdown_all().await?;
                Ok(serde_json::to_value(ShutdownResponse {})?)
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
                self.ensure_root_subscription(thread_id).await?;
                Ok(serde_json::to_value(ThreadResumeResponse {
                    thread_id: resumed.thread_id.to_string(),
                    rollout_path: resumed.rollout_path.display().to_string(),
                    initial_messages: resumed.initial_messages,
                })?)
            }
            ClientRequest::ThreadPreview { params, .. } => {
                tracing::debug!(
                    thread_id = %params.thread_id,
                    "processing thread preview request"
                );
                let thread_id = params
                    .thread_id
                    .parse::<ThreadId>()
                    .map_err(AppServerError::invalid_thread_id)?;
                // Subscribe before snapshotting: core persists events before
                // broadcasting, so this ordering can only duplicate persisted
                // events (the client dedupes), never lose them. `NotLive` is
                // the expected static-preview path for completed threads.
                let outcome = self
                    .ensure_thread_subscription(thread_id, SubscriptionOwner::Preview)
                    .await?;
                let watcher_taken = outcome != SubscriptionOutcome::NotLive;
                let preview = match self.threads.preview_thread(thread_id).await {
                    Ok(preview) => preview,
                    Err(err) => {
                        // No view will be pushed client-side, so no unwatch
                        // would ever release the watcher we just took.
                        if watcher_taken {
                            self.release_preview_watcher(thread_id).await;
                        }
                        return Err(err.into());
                    }
                };
                Ok(serde_json::to_value(ThreadPreviewResponse {
                    thread_id: preview.thread_id.to_string(),
                    agent_path: preview.agent_path,
                    agent_nickname: preview.agent_nickname,
                    status: preview.status,
                    is_live: preview.is_live,
                    initial_messages: preview.initial_messages,
                })?)
            }
            ClientRequest::ThreadUnwatch { params, .. } => {
                tracing::debug!(
                    thread_id = %params.thread_id,
                    "processing thread unwatch request"
                );
                let thread_id = params
                    .thread_id
                    .parse::<ThreadId>()
                    .map_err(AppServerError::invalid_thread_id)?;
                self.release_preview_watcher(thread_id).await;
                Ok(serde_json::to_value(ThreadUnwatchResponse {
                    thread_id: thread_id.to_string(),
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

    /// Subscribe to a thread's event channel and forward its events to the
    /// client, tracking who owns the subscription. The map entry is inserted
    /// only after both the core subscription and the forward-task spawn
    /// succeed (the lock is held across the attempt so concurrent calls
    /// cannot race); a thread with no live channel is reported as `NotLive`
    /// rather than silently ignored.
    async fn ensure_thread_subscription(
        &self,
        thread_id: ThreadId,
        owner: SubscriptionOwner,
    ) -> AppServerResult<SubscriptionOutcome> {
        let mut subscribed = self.subscribed_threads.lock().await;
        if let Some(entry) = subscribed.get_mut(&thread_id) {
            match owner {
                SubscriptionOwner::Root => entry.root = true,
                SubscriptionOwner::Preview => entry.preview_watchers += 1,
            }
            return Ok(SubscriptionOutcome::AlreadySubscribed);
        }

        let mut rx = match self.threads.subscribe(thread_id).await {
            Ok(rx) => rx,
            Err(CoreError::UnknownThread { .. }) => return Ok(SubscriptionOutcome::NotLive),
            Err(err) => return Err(err.into()),
        };
        let event_tx = self.event_tx.clone();
        let subscription_span = tracing::info_span!(
            "app_server.session_event_subscription",
            thread_id = %thread_id,
        );
        let task = tokio::task::Builder::new()
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
            .map_err(|err| {
                AppServerError::Core(CoreError::TaskSpawn {
                    task_name: "app_server.session_subscription",
                    source: err,
                })
            })?;

        subscribed.insert(
            thread_id,
            ThreadSubscription {
                abort: task.abort_handle(),
                root: owner == SubscriptionOwner::Root,
                preview_watchers: usize::from(owner == SubscriptionOwner::Preview),
            },
        );
        Ok(SubscriptionOutcome::Subscribed)
    }

    /// Subscribe with root ownership; the thread was just started or resumed,
    /// so a missing live channel is an error rather than an expected outcome.
    async fn ensure_root_subscription(&self, thread_id: ThreadId) -> AppServerResult<()> {
        match self
            .ensure_thread_subscription(thread_id, SubscriptionOwner::Root)
            .await?
        {
            SubscriptionOutcome::Subscribed | SubscriptionOutcome::AlreadySubscribed => Ok(()),
            SubscriptionOutcome::NotLive => {
                Err(AppServerError::Core(CoreError::UnknownThread { thread_id }))
            }
        }
    }

    /// Release one preview watcher; the subscription is dropped only when no
    /// preview watcher remains and the root session does not own it.
    async fn release_preview_watcher(&self, thread_id: ThreadId) {
        let mut subscribed = self.subscribed_threads.lock().await;
        let Some(entry) = subscribed.get_mut(&thread_id) else {
            return;
        };
        entry.preview_watchers = entry.preview_watchers.saturating_sub(1);
        if entry.preview_watchers == 0 && !entry.root {
            entry.abort.abort();
            subscribed.remove(&thread_id);
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
    use std::{path::PathBuf, sync::Arc};

    use app_server_protocol::{
        AskUserQuestionParams, ClientRequest, RequestId, ShutdownParams, ShutdownResponse,
        ThreadPreviewParams, ThreadPreviewResponse, ThreadStartParams, ThreadUnwatchParams,
        TurnCancelParams, TurnCancelResponse, TurnStartParams,
    };
    use tokio::sync::{mpsc, mpsc::error::TryRecvError};

    use super::{CoreMessageProcessor, ask_user_client};
    use crate::{
        error_code::INVALID_PARAMS_ERROR_CODE, in_process::InProcessServerEvent,
        outgoing_message::OutgoingMessageSender,
    };

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    struct CwdRestore(PathBuf);

    impl Drop for CwdRestore {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.0);
        }
    }

    #[tokio::test]
    async fn invalid_thread_id_request_preserves_structured_error_info() -> TestResult {
        let _cwd_guard = crate::cwd_test_lock().lock().await;
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

    #[tokio::test]
    async fn invalid_turn_cancel_thread_id_preserves_structured_error_info() -> TestResult {
        let _cwd_guard = crate::cwd_test_lock().lock().await;
        let workspace = tempfile::TempDir::new()?;
        let original_cwd = std::env::current_dir()?;
        std::env::set_current_dir(workspace.path())?;
        let _cwd_restore = CwdRestore(original_cwd);

        let (event_tx, _event_rx) = mpsc::channel::<InProcessServerEvent>(8);
        let outgoing = Arc::new(OutgoingMessageSender::new(event_tx.clone()));
        let processor = CoreMessageProcessor::new(event_tx, outgoing).await?;
        let request = ClientRequest::TurnCancel {
            request_id: RequestId(1),
            params: TurnCancelParams {
                thread_id: "not-a-thread-id".to_string(),
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

    #[tokio::test]
    async fn turn_cancel_returns_cancelled_thread_ids() -> TestResult {
        let _cwd_guard = crate::cwd_test_lock().lock().await;
        let workspace = tempfile::TempDir::new()?;
        let original_cwd = std::env::current_dir()?;
        std::env::set_current_dir(workspace.path())?;
        let _cwd_restore = CwdRestore(original_cwd);

        let (event_tx, _event_rx) = mpsc::channel::<InProcessServerEvent>(8);
        let outgoing = Arc::new(OutgoingMessageSender::new(event_tx.clone()));
        let processor = CoreMessageProcessor::new(event_tx, outgoing).await?;
        let started_value = processor
            .process_request(ClientRequest::ThreadStart {
                request_id: RequestId(1),
                params: ThreadStartParams::default(),
            })
            .await?;
        let started: app_server_protocol::ThreadStartResponse =
            serde_json::from_value(started_value)?;

        let value = processor
            .process_request(ClientRequest::TurnCancel {
                request_id: RequestId(2),
                params: TurnCancelParams {
                    thread_id: started.thread_id.clone(),
                },
            })
            .await?;
        let response: TurnCancelResponse = serde_json::from_value(value)?;

        assert_eq!(response.thread_id, started.thread_id);
        assert!(response.cancelled_thread_ids.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn shutdown_resolves_and_clears_threads() -> TestResult {
        let _cwd_guard = crate::cwd_test_lock().lock().await;
        let workspace = tempfile::TempDir::new()?;
        let original_cwd = std::env::current_dir()?;
        std::env::set_current_dir(workspace.path())?;
        let _cwd_restore = CwdRestore(original_cwd);

        let (event_tx, _event_rx) = mpsc::channel::<InProcessServerEvent>(8);
        let outgoing = Arc::new(OutgoingMessageSender::new(event_tx.clone()));
        let processor = CoreMessageProcessor::new(event_tx, outgoing).await?;
        let started_value = processor
            .process_request(ClientRequest::ThreadStart {
                request_id: RequestId(1),
                params: ThreadStartParams::default(),
            })
            .await?;
        let started: app_server_protocol::ThreadStartResponse =
            serde_json::from_value(started_value)?;

        let value = processor
            .process_request(ClientRequest::Shutdown {
                request_id: RequestId(2),
                params: ShutdownParams {},
            })
            .await?;
        let _response: ShutdownResponse = serde_json::from_value(value)?;

        // The thread map is cleared: the shut-down thread is gone.
        let Err(error) = processor
            .process_request(ClientRequest::TurnStart {
                request_id: RequestId(3),
                params: TurnStartParams {
                    thread_id: started.thread_id,
                    input: "hello".to_string(),
                },
            })
            .await
        else {
            panic!("turn start after shutdown should fail");
        };
        let info = error.data.as_ref().ok_or("missing error data")?;
        assert_eq!(info.kind, "unknown_thread");
        Ok(())
    }

    #[tokio::test]
    async fn ask_user_client_routes_from_params_thread_id() -> TestResult {
        let (event_tx, mut event_rx) = mpsc::channel::<InProcessServerEvent>(8);
        let outgoing = Arc::new(OutgoingMessageSender::new(event_tx));
        let client = ask_user_client(outgoing);

        let Err(error) = client
            .ask(AskUserQuestionParams {
                thread_id: "not-a-thread-id".to_string(),
                turn_id: "turn-1".to_string(),
                questions: Vec::new(),
            })
            .await
        else {
            panic!("invalid ask_user_question thread id should fail");
        };

        assert_eq!(error.code, INVALID_PARAMS_ERROR_CODE);
        assert!(matches!(event_rx.try_recv(), Err(TryRecvError::Empty)));
        Ok(())
    }

    type TestResult2<T> = Result<T, Box<dyn std::error::Error>>;

    /// Returns the processor plus the live event receiver — the receiver must
    /// stay alive or the forward tasks exit as soon as they send.
    async fn test_processor()
    -> TestResult2<(CoreMessageProcessor, mpsc::Receiver<InProcessServerEvent>)> {
        let (event_tx, event_rx) = mpsc::channel::<InProcessServerEvent>(64);
        let outgoing = Arc::new(OutgoingMessageSender::new(event_tx.clone()));
        Ok((
            CoreMessageProcessor::new(event_tx, outgoing).await?,
            event_rx,
        ))
    }

    async fn preview(
        processor: &CoreMessageProcessor,
        thread_id: &str,
    ) -> Result<ThreadPreviewResponse, app_server_protocol::JsonRpcError> {
        let value = processor
            .process_request(ClientRequest::ThreadPreview {
                request_id: RequestId(90),
                params: ThreadPreviewParams {
                    thread_id: thread_id.to_string(),
                },
            })
            .await?;
        serde_json::from_value(value).map_err(|err| {
            app_server_protocol::JsonRpcError::new(
                -32000,
                smooth_protocol::ErrorInfo::new("test_decode", err.to_string()),
            )
        })
    }

    async fn unwatch(processor: &CoreMessageProcessor, thread_id: &str) -> TestResult {
        processor
            .process_request(ClientRequest::ThreadUnwatch {
                request_id: RequestId(91),
                params: ThreadUnwatchParams {
                    thread_id: thread_id.to_string(),
                },
            })
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn thread_preview_live_thread_takes_watcher_and_unwatch_never_aborts_root() -> TestResult
    {
        let _cwd_guard = crate::cwd_test_lock().lock().await;
        let workspace = tempfile::TempDir::new()?;
        let original_cwd = std::env::current_dir()?;
        std::env::set_current_dir(workspace.path())?;
        let _cwd_restore = CwdRestore(original_cwd);

        let (processor, _event_rx) = test_processor().await?;
        let started_value = processor
            .process_request(ClientRequest::ThreadStart {
                request_id: RequestId(1),
                params: ThreadStartParams::default(),
            })
            .await?;
        let started: app_server_protocol::ThreadStartResponse =
            serde_json::from_value(started_value)?;
        let thread_id = started.thread_id.parse::<smooth_protocol::ThreadId>()?;

        let response = preview(&processor, &started.thread_id).await?;
        assert_eq!(response.thread_id, started.thread_id);
        assert!(response.is_live);

        {
            let subscribed = processor.subscribed_threads.lock().await;
            let entry = subscribed.get(&thread_id).ok_or("missing subscription")?;
            assert!(entry.root, "root start owns the subscription");
            assert_eq!(entry.preview_watchers, 1);
        }

        unwatch(&processor, &started.thread_id).await?;
        let subscribed = processor.subscribed_threads.lock().await;
        let entry = subscribed
            .get(&thread_id)
            .ok_or("root subscription must survive preview unwatch")?;
        assert!(entry.root);
        assert_eq!(entry.preview_watchers, 0);
        assert!(!entry.abort.is_finished());
        Ok(())
    }

    #[tokio::test]
    async fn thread_preview_refcounts_and_last_unwatch_drops_preview_only_subscription()
    -> TestResult {
        let _cwd_guard = crate::cwd_test_lock().lock().await;
        let workspace = tempfile::TempDir::new()?;
        let original_cwd = std::env::current_dir()?;
        std::env::set_current_dir(workspace.path())?;
        let _cwd_restore = CwdRestore(original_cwd);

        let (processor, _event_rx) = test_processor().await?;
        // Live thread with no root subscription: created directly on the
        // thread manager, not via the ThreadStart request.
        let started = processor.threads.start_thread().await?;
        let thread_id = started.thread_id;
        let thread_id_str = thread_id.to_string();

        let first = preview(&processor, &thread_id_str).await?;
        assert!(first.is_live);
        let _second = preview(&processor, &thread_id_str).await?;
        {
            let subscribed = processor.subscribed_threads.lock().await;
            let entry = subscribed.get(&thread_id).ok_or("missing subscription")?;
            assert!(!entry.root);
            assert_eq!(entry.preview_watchers, 2);
        }

        unwatch(&processor, &thread_id_str).await?;
        {
            let subscribed = processor.subscribed_threads.lock().await;
            let entry = subscribed
                .get(&thread_id)
                .ok_or("subscription must survive while a watcher remains")?;
            assert_eq!(entry.preview_watchers, 1);
        }

        unwatch(&processor, &thread_id_str).await?;
        let subscribed = processor.subscribed_threads.lock().await;
        assert!(
            !subscribed.contains_key(&thread_id),
            "last unwatch drops a preview-only subscription"
        );
        Ok(())
    }

    #[tokio::test]
    async fn thread_preview_completed_thread_is_static_without_subscription() -> TestResult {
        let _cwd_guard = crate::cwd_test_lock().lock().await;
        let workspace = tempfile::TempDir::new()?;
        let original_cwd = std::env::current_dir()?;
        std::env::set_current_dir(workspace.path())?;
        let _cwd_restore = CwdRestore(original_cwd);

        let (processor, _event_rx) = test_processor().await?;
        // Force the static path: the thread is fully gone from the live map
        // before the preview request, leaving only its rollout.
        let started = processor.threads.start_thread().await?;
        let thread_id = started.thread_id;
        processor.threads.shutdown_all().await?;

        let response = preview(&processor, &thread_id.to_string()).await?;
        assert!(!response.is_live);
        let subscribed = processor.subscribed_threads.lock().await;
        assert!(
            !subscribed.contains_key(&thread_id),
            "static preview must not leave a subscription entry"
        );
        Ok(())
    }

    #[tokio::test]
    async fn thread_preview_failure_rolls_back_just_taken_watcher() -> TestResult {
        let _cwd_guard = crate::cwd_test_lock().lock().await;
        let workspace = tempfile::TempDir::new()?;
        let original_cwd = std::env::current_dir()?;
        std::env::set_current_dir(workspace.path())?;
        let _cwd_restore = CwdRestore(original_cwd);

        let (processor, _event_rx) = test_processor().await?;
        let started = processor.threads.start_thread().await?;
        let thread_id = started.thread_id;
        // Live thread whose rollout cannot be read: subscribe succeeds, the
        // snapshot fails, and the watcher must be rolled back.
        std::fs::remove_file(&started.rollout_path)?;

        assert!(preview(&processor, &thread_id.to_string()).await.is_err());
        let subscribed = processor.subscribed_threads.lock().await;
        assert!(
            !subscribed.contains_key(&thread_id),
            "failed preview must roll back the watcher it took"
        );
        Ok(())
    }

    #[tokio::test]
    async fn thread_preview_unknown_thread_errors_without_leaking_subscription() -> TestResult {
        let _cwd_guard = crate::cwd_test_lock().lock().await;
        let workspace = tempfile::TempDir::new()?;
        let original_cwd = std::env::current_dir()?;
        std::env::set_current_dir(workspace.path())?;
        let _cwd_restore = CwdRestore(original_cwd);

        let (processor, _event_rx) = test_processor().await?;
        let unknown = smooth_protocol::ThreadId::new();
        assert!(preview(&processor, &unknown.to_string()).await.is_err());
        let subscribed = processor.subscribed_threads.lock().await;
        assert!(subscribed.is_empty());
        Ok(())
    }
}
