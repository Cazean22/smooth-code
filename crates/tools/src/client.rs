use std::sync::Arc;

use app_server_protocol::{
    AskUserQuestionParams, AskUserQuestionResponse, DynamicToolCallParams, JSONRPCErrorError,
};
use futures_util::future::BoxFuture;

pub trait DynamicToolClient: Send + Sync {
    fn call(
        &self,
        params: DynamicToolCallParams,
    ) -> BoxFuture<'static, Result<serde_json::Value, JSONRPCErrorError>>;

    fn abort_pending_server_requests(&self) -> BoxFuture<'static, ()>;
}

pub trait DynamicToolClientFactory: Send + Sync {
    fn build(&self, thread_id: smooth_protocol::ThreadId) -> Arc<dyn DynamicToolClient>;
}

pub trait AskUserClient: Send + Sync {
    fn ask(
        &self,
        params: AskUserQuestionParams,
    ) -> BoxFuture<'static, Result<AskUserQuestionResponse, JSONRPCErrorError>>;

    fn abort_pending_server_requests(&self) -> BoxFuture<'static, ()>;
}

pub trait AskUserClientFactory: Send + Sync {
    fn build(&self, thread_id: smooth_protocol::ThreadId) -> Arc<dyn AskUserClient>;
}
