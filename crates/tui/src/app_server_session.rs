use app_server::in_process::InProcessServerEvent;
use app_server_protocol::{
    ClientRequest, RequestId, SetPlanModeParams, SetPlanModeResponse, ShutdownParams,
    ShutdownResponse, ThreadListParams, ThreadListResponse, ThreadPreviewParams,
    ThreadPreviewResponse, ThreadResumeParams, ThreadResumeResponse, ThreadStartParams,
    ThreadStartResponse, ThreadUnwatchParams, ThreadUnwatchResponse, TurnCancelParams,
    TurnCancelResponse, TurnStartParams, TurnStartResponse,
};
use smooth_protocol::ThreadId;

use crate::app_server_client::AppServerClient;
use crate::error::TuiResult;
use crate::project_instructions::load_project_instructions;

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
    pub(crate) async fn start_thread(&mut self) -> TuiResult<ThreadStartResponse> {
        let project_instructions = load_project_instructions()?;
        let request = ClientRequest::ThreadStart {
            request_id: RequestId(self.next_request_id as usize),
            params: ThreadStartParams {
                project_instructions,
            },
        };
        self.next_request_id += 1;
        let value = self.client.request(request).await?;
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
    ) -> TuiResult<TurnStartResponse> {
        let request = ClientRequest::TurnStart {
            request_id: RequestId(self.next_request_id as usize),
            params: TurnStartParams {
                thread_id: thread_id.to_string(),
                input,
            },
        };
        self.next_request_id += 1;
        let value = self.client.request(request).await?;
        Ok(serde_json::from_value(value)?)
    }

    #[tracing::instrument(name = "tui.turn_cancel", skip(self), fields(thread_id = %thread_id))]
    pub(crate) async fn turn_cancel(
        &mut self,
        thread_id: ThreadId,
    ) -> TuiResult<TurnCancelResponse> {
        let request = ClientRequest::TurnCancel {
            request_id: RequestId(self.next_request_id as usize),
            params: TurnCancelParams {
                thread_id: thread_id.to_string(),
            },
        };
        self.next_request_id += 1;
        let value = self.client.request(request).await?;
        Ok(serde_json::from_value(value)?)
    }

    /// Ask the server to shut every thread down gracefully (cancel turns,
    /// kill tool subprocesses). Called once from the TUI's exit epilogue.
    #[tracing::instrument(name = "tui.shutdown", skip(self))]
    pub(crate) async fn shutdown(&mut self) -> TuiResult<ShutdownResponse> {
        let request = ClientRequest::Shutdown {
            request_id: RequestId(self.next_request_id as usize),
            params: ShutdownParams {},
        };
        self.next_request_id += 1;
        let value = self.client.request(request).await?;
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
    ) -> TuiResult<SetPlanModeResponse> {
        let request = ClientRequest::SetPlanMode {
            request_id: RequestId(self.next_request_id as usize),
            params: SetPlanModeParams {
                thread_id: thread_id.to_string(),
                enabled,
            },
        };
        self.next_request_id += 1;
        let value = self.client.request(request).await?;
        Ok(serde_json::from_value(value)?)
    }

    #[tracing::instrument(name = "tui.thread_resume", skip(self), fields(thread_id = %thread_id))]
    #[allow(dead_code)]
    pub(crate) async fn thread_resume(
        &mut self,
        thread_id: ThreadId,
    ) -> TuiResult<ThreadResumeResponse> {
        let request = ClientRequest::ThreadResume {
            request_id: RequestId(self.next_request_id as usize),
            params: ThreadResumeParams {
                thread_id: thread_id.to_string(),
            },
        };
        self.next_request_id += 1;
        let value = self.client.request(request).await?;
        Ok(serde_json::from_value(value)?)
    }

    #[tracing::instrument(name = "tui.thread_list", skip(self))]
    #[allow(dead_code)]
    pub(crate) async fn thread_list(
        &mut self,
        cursor: Option<String>,
        limit: Option<u32>,
    ) -> TuiResult<ThreadListResponse> {
        let request = ClientRequest::ThreadList {
            request_id: RequestId(self.next_request_id as usize),
            params: ThreadListParams { cursor, limit },
        };
        self.next_request_id += 1;
        let value = self.client.request(request).await?;
        Ok(serde_json::from_value(value)?)
    }

    #[tracing::instrument(name = "tui.thread_preview", skip(self), fields(thread_id = %thread_id))]
    pub(crate) async fn thread_preview(
        &mut self,
        thread_id: ThreadId,
    ) -> TuiResult<ThreadPreviewResponse> {
        let request = ClientRequest::ThreadPreview {
            request_id: RequestId(self.next_request_id as usize),
            params: ThreadPreviewParams {
                thread_id: thread_id.to_string(),
            },
        };
        self.next_request_id += 1;
        let value = self.client.request(request).await?;
        Ok(serde_json::from_value(value)?)
    }

    #[tracing::instrument(name = "tui.thread_unwatch", skip(self), fields(thread_id = %thread_id))]
    pub(crate) async fn thread_unwatch(
        &mut self,
        thread_id: ThreadId,
    ) -> TuiResult<ThreadUnwatchResponse> {
        let request = ClientRequest::ThreadUnwatch {
            request_id: RequestId(self.next_request_id as usize),
            params: ThreadUnwatchParams {
                thread_id: thread_id.to_string(),
            },
        };
        self.next_request_id += 1;
        let value = self.client.request(request).await?;
        Ok(serde_json::from_value(value)?)
    }

    pub(crate) async fn next_event(&mut self) -> Option<InProcessServerEvent> {
        self.client.next_event().await
    }

    pub(crate) async fn respond_to_server_request(
        &self,
        request_id: RequestId,
        result: serde_json::Value,
    ) -> TuiResult<()> {
        self.client
            .respond_to_server_request(request_id, result)
            .await
    }

    pub(crate) async fn fail_server_request(
        &self,
        request_id: RequestId,
        error: app_server_protocol::JsonRpcError,
    ) -> TuiResult<()> {
        self.client.fail_server_request(request_id, error).await
    }
}
