use app_server::in_process::{
    self, InProcessClientHandle, InProcessServerEvent, InProcessStartArgs,
};
use app_server_protocol::{ClientCommand, ClientRequest, JSONRPCErrorError};
use tokio::sync::oneshot;

pub(crate) struct AppServerClient {
    handle: InProcessClientHandle,
}

impl AppServerClient {
    pub(crate) async fn start(channel_capacity: usize) -> anyhow::Result<Self> {
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
    ) -> std::result::Result<serde_json::Value, JSONRPCErrorError> {
        let request_name = match &request {
            ClientRequest::ThreadStart { .. } => "thread_start",
            ClientRequest::TurnStart { .. } => "turn_start",
            ClientRequest::ThreadResume { .. } => "thread_resume",
            ClientRequest::ThreadList { .. } => "thread_list",
        };
        tracing::Span::current().record("request", request_name);

        let (response_tx, response_rx) = oneshot::channel();
        let command = ClientCommand::Request {
            request: Box::new(request),
            response_tx,
        };
        self.handle
            .client_tx
            .send(command)
            .await
            .map_err(|err| JSONRPCErrorError {
                code: -32000,
                data: None,
                message: err.to_string(),
            })?;
        response_rx.await.map_err(|err| JSONRPCErrorError {
            code: -32000,
            data: None,
            message: err.to_string(),
        })?
    }

    pub(crate) async fn next_event(&mut self) -> Option<InProcessServerEvent> {
        self.handle.next_event().await
    }

    pub(crate) async fn respond_to_server_request(
        &self,
        request_id: app_server_protocol::RequestId,
        result: serde_json::Value,
    ) -> anyhow::Result<()> {
        self.handle
            .client_tx
            .send(ClientCommand::ServerRequestResponse { request_id, result })
            .await?;
        Ok(())
    }

    pub(crate) async fn fail_server_request(
        &self,
        request_id: app_server_protocol::RequestId,
        error: JSONRPCErrorError,
    ) -> anyhow::Result<()> {
        self.handle
            .client_tx
            .send(ClientCommand::ServerRequestError { request_id, error })
            .await?;
        Ok(())
    }
}
