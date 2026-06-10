use std::{future::Future, sync::Arc};

use app_server_protocol::{
    AskUserQuestionParams, AskUserQuestionResponse, JsonRpcError, RequestPlanApprovalParams,
    RequestPlanApprovalResponse,
};
use futures_util::future::BoxFuture;
use smooth_protocol::ThreadId;

type AskUserFuture = BoxFuture<'static, Result<AskUserQuestionResponse, JsonRpcError>>;
type AbortPendingFuture = BoxFuture<'static, ()>;
type PlanApprovalFuture = BoxFuture<'static, Result<RequestPlanApprovalResponse, JsonRpcError>>;

#[derive(Clone)]
pub struct AskUserClient {
    ask: Arc<dyn Fn(AskUserQuestionParams) -> AskUserFuture + Send + Sync>,
    abort_pending_server_requests: Arc<dyn Fn(ThreadId) -> AbortPendingFuture + Send + Sync>,
    request_plan_approval:
        Arc<dyn Fn(RequestPlanApprovalParams) -> PlanApprovalFuture + Send + Sync>,
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
            request_plan_approval: Arc::new(|_params| {
                Box::pin(async {
                    Err(JsonRpcError {
                        code: -32601,
                        data: None,
                        message: "plan approval is not supported by this client".to_string(),
                    })
                })
            }),
        }
    }

    /// Attach a plan-approval handler. Clients that cannot present a plan for
    /// approval keep the default handler, which fails the request.
    pub fn with_plan_approval<ApproveFn, ApproveFut>(mut self, request_plan_approval: ApproveFn) -> Self
    where
        ApproveFn: Fn(RequestPlanApprovalParams) -> ApproveFut + Send + Sync + 'static,
        ApproveFut:
            Future<Output = Result<RequestPlanApprovalResponse, JsonRpcError>> + Send + 'static,
    {
        self.request_plan_approval =
            Arc::new(move |params| Box::pin(request_plan_approval(params)));
        self
    }

    pub fn ask(
        &self,
        params: AskUserQuestionParams,
    ) -> BoxFuture<'static, Result<AskUserQuestionResponse, JsonRpcError>> {
        (self.ask)(params)
    }

    pub fn request_plan_approval(
        &self,
        params: RequestPlanApprovalParams,
    ) -> BoxFuture<'static, Result<RequestPlanApprovalResponse, JsonRpcError>> {
        (self.request_plan_approval)(params)
    }

    pub fn abort_pending_server_requests(&self, thread_id: ThreadId) -> BoxFuture<'static, ()> {
        (self.abort_pending_server_requests)(thread_id)
    }
}
