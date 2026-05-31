use std::{future::Future, sync::Arc};

use app_server_protocol::{AskUserQuestionParams, AskUserQuestionResponse, JsonRpcError};
use futures_util::future::BoxFuture;
use smooth_protocol::ThreadId;

type AskUserFuture = BoxFuture<'static, Result<AskUserQuestionResponse, JsonRpcError>>;
type AbortPendingFuture = BoxFuture<'static, ()>;

#[derive(Clone)]
pub struct AskUserClient {
    ask: Arc<dyn Fn(AskUserQuestionParams) -> AskUserFuture + Send + Sync>,
    abort_pending_server_requests: Arc<dyn Fn(ThreadId) -> AbortPendingFuture + Send + Sync>,
}

impl AskUserClient {
    pub fn new<AskFn, AskFut, AbortFn, AbortFut>(
        ask: AskFn,
        abort_pending_server_requests: AbortFn,
    ) -> Self
    where
        AskFn: Fn(AskUserQuestionParams) -> AskFut + Send + Sync + 'static,
        AskFut: Future<Output = Result<AskUserQuestionResponse, JsonRpcError>> + Send + 'static,
        AbortFn: Fn(ThreadId) -> AbortFut + Send + Sync + 'static,
        AbortFut: Future<Output = ()> + Send + 'static,
    {
        Self {
            ask: Arc::new(move |params| Box::pin(ask(params))),
            abort_pending_server_requests: Arc::new(move |thread_id| {
                Box::pin(abort_pending_server_requests(thread_id))
            }),
        }
    }

    pub fn ask(
        &self,
        params: AskUserQuestionParams,
    ) -> BoxFuture<'static, Result<AskUserQuestionResponse, JsonRpcError>> {
        (self.ask)(params)
    }

    pub fn abort_pending_server_requests(&self, thread_id: ThreadId) -> BoxFuture<'static, ()> {
        (self.abort_pending_server_requests)(thread_id)
    }
}
