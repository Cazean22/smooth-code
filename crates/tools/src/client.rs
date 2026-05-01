use std::sync::Arc;

use app_server_protocol::{DynamicToolCallParams, JSONRPCErrorError};
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
