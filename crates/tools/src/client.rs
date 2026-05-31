use std::sync::Arc;

use app_server_protocol::{AskUserQuestionParams, AskUserQuestionResponse, JsonRpcError};
use futures_util::future::BoxFuture;

pub trait AskUserClient: Send + Sync {
    fn ask(
        &self,
        params: AskUserQuestionParams,
    ) -> BoxFuture<'static, Result<AskUserQuestionResponse, JsonRpcError>>;

    fn abort_pending_server_requests(&self) -> BoxFuture<'static, ()>;
}

pub trait AskUserClientFactory: Send + Sync {
    fn build(&self, thread_id: smooth_protocol::ThreadId) -> Arc<dyn AskUserClient>;
}
