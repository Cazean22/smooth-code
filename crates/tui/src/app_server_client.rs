use app_server::in_process::{
    self, InProcessClientHandle, InProcessServerEvent, InProcessStartArgs,
};
use app_server_protocol::{ClientCommand, ClientRequest, JsonRpcError};
use smooth_protocol::ErrorInfo;
use tokio::sync::oneshot;

use crate::error::TuiResult;

pub(crate) struct AppServerClient {
    handle: InProcessClientHandle,
}

impl AppServerClient {
    pub(crate) async fn start(channel_capacity: usize) -> TuiResult<Self> {
        Ok(Self {
            handle: in_process::start(InProcessStartArgs { channel_capacity }).await?,
        })
    }

    #[tracing::instrument(
        name = "tui.app_server.request",
        skip(self, request),
        fields(request = tracing::field::Empty)
    )]
    pub(crate) async fn request(
        &self,
        request: ClientRequest,
    ) -> std::result::Result<serde_json::Value, JsonRpcError> {
        let request_name = match &request {
            ClientRequest::ThreadStart { .. } => "thread_start",
            ClientRequest::TurnStart { .. } => "turn_start",
            ClientRequest::TurnCancel { .. } => "turn_cancel",
            ClientRequest::Shutdown { .. } => "shutdown",
            ClientRequest::ThreadResume { .. } => "thread_resume",
            ClientRequest::ThreadPreview { .. } => "thread_preview",
            ClientRequest::ThreadUnwatch { .. } => "thread_unwatch",
            ClientRequest::ThreadList { .. } => "thread_list",
            ClientRequest::SetPlanMode { .. } => "set_plan_mode",
        };
        tracing::Span::current().record("request", request_name);

        let (response_tx, response_rx) = oneshot::channel();
        let command = ClientCommand::Request {
            request: Box::new(request),
            response_tx,
        };
        self.handle.client_tx.send(command).await.map_err(|err| {
            JsonRpcError::new(
                -32000,
                ErrorInfo::new("request_channel_closed", err.to_string()).with_source("smooth-tui"),
            )
        })?;
        response_rx.await.map_err(|err| {
            JsonRpcError::new(
                -32000,
                ErrorInfo::new("response_channel_closed", err.to_string())
                    .with_source("smooth-tui"),
            )
        })?
    }

    pub(crate) async fn next_event(&mut self) -> Option<InProcessServerEvent> {
        self.handle.next_event().await
    }

    pub(crate) async fn respond_to_server_request(
        &self,
        request_id: app_server_protocol::RequestId,
        result: serde_json::Value,
    ) -> TuiResult<()> {
        self.handle
            .client_tx
            .send(ClientCommand::ServerRequestResponse { request_id, result })
            .await
            .map_err(|err| {
                JsonRpcError::new(
                    -32000,
                    ErrorInfo::new("request_channel_closed", err.to_string())
                        .with_source("smooth-tui"),
                )
            })?;
        Ok(())
    }

    pub(crate) async fn fail_server_request(
        &self,
        request_id: app_server_protocol::RequestId,
        error: JsonRpcError,
    ) -> TuiResult<()> {
        self.handle
            .client_tx
            .send(ClientCommand::ServerRequestError { request_id, error })
            .await
            .map_err(|err| {
                JsonRpcError::new(
                    -32000,
                    ErrorInfo::new("request_channel_closed", err.to_string())
                        .with_source("smooth-tui"),
                )
            })?;
        Ok(())
    }
}
