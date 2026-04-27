use anyhow::Result;
use app_server::in_process::InProcessServerEvent;
use app_server_protocol::{
    ClientRequest, RequestId, ThreadStartParams, ThreadStartResponse, TurnStartParams,
    TurnStartResponse,
};
use smooth_protocol::ThreadId;

use crate::app_server_client::AppServerClient;

pub(crate) struct AppServerSession {
    client: AppServerClient,
    next_request_id: i64,
}

impl AppServerSession {
    pub(crate) fn new(client: AppServerClient) -> Self {
        Self {
            client,
            next_request_id: 1,
        }
    }

    pub(crate) async fn start_thread(&mut self) -> Result<ThreadStartResponse> {
        let request = ClientRequest::ThreadStart {
            request_id: RequestId(self.next_request_id as usize),
            params: ThreadStartParams::default(),
        };
        self.next_request_id += 1;
        let value = self
            .client
            .request(request)
            .await
            .map_err(|err| anyhow::anyhow!(err.message))?;
        Ok(serde_json::from_value(value)?)
    }

    pub(crate) async fn turn_start(
        &mut self,
        thread_id: ThreadId,
        input: String,
    ) -> Result<TurnStartResponse> {
        let request = ClientRequest::TurnStart {
            request_id: RequestId(self.next_request_id as usize),
            params: TurnStartParams {
                thread_id: thread_id.to_string(),
                input,
            },
        };
        self.next_request_id += 1;
        let value = self
            .client
            .request(request)
            .await
            .map_err(|err| anyhow::anyhow!(err.message))?;
        Ok(serde_json::from_value(value)?)
    }

    pub(crate) async fn next_event(&mut self) -> Option<InProcessServerEvent> {
        self.client.next_event().await
    }
}
