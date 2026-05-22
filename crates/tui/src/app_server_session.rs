use anyhow::Result;
use app_server::in_process::InProcessServerEvent;
use app_server_protocol::{
    ClientRequest, RequestId, SetPlanModeParams, SetPlanModeResponse, ThreadListParams,
    ThreadListResponse, ThreadResumeParams, ThreadResumeResponse, ThreadStartParams,
    ThreadStartResponse, TurnStartParams, TurnStartResponse,
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

    #[tracing::instrument(name = "tui.thread_start", skip(self))]
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

    #[tracing::instrument(
        name = "tui.turn_start",
        skip(self, input),
        fields(thread_id = %thread_id, input_len = input.len())
    )]
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

    #[tracing::instrument(
        name = "tui.set_plan_mode",
        skip(self),
        fields(thread_id = %thread_id, enabled = enabled)
    )]
    pub(crate) async fn set_plan_mode(
        &mut self,
        thread_id: ThreadId,
        enabled: bool,
    ) -> Result<SetPlanModeResponse> {
        let request = ClientRequest::SetPlanMode {
            request_id: RequestId(self.next_request_id as usize),
            params: SetPlanModeParams {
                thread_id: thread_id.to_string(),
                enabled,
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

    #[tracing::instrument(name = "tui.thread_resume", skip(self), fields(thread_id = %thread_id))]
    #[allow(dead_code)]
    pub(crate) async fn thread_resume(
        &mut self,
        thread_id: ThreadId,
    ) -> Result<ThreadResumeResponse> {
        let request = ClientRequest::ThreadResume {
            request_id: RequestId(self.next_request_id as usize),
            params: ThreadResumeParams {
                thread_id: thread_id.to_string(),
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

    #[tracing::instrument(name = "tui.thread_list", skip(self))]
    #[allow(dead_code)]
    pub(crate) async fn thread_list(
        &mut self,
        cursor: Option<String>,
        limit: Option<u32>,
    ) -> Result<ThreadListResponse> {
        let request = ClientRequest::ThreadList {
            request_id: RequestId(self.next_request_id as usize),
            params: ThreadListParams { cursor, limit },
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

    pub(crate) async fn respond_to_server_request(
        &self,
        request_id: RequestId,
        result: serde_json::Value,
    ) -> Result<()> {
        self.client
            .respond_to_server_request(request_id, result)
            .await
    }

    pub(crate) async fn fail_server_request(
        &self,
        request_id: RequestId,
        error: app_server_protocol::JSONRPCErrorError,
    ) -> Result<()> {
        self.client.fail_server_request(request_id, error).await
    }
}
