use std::sync::Arc;

use app_server_protocol::{ClientRequest, TurnStartResponse};
use smooth_core::ThreadManagerState;
use smooth_protocol::ThreadId;

use crate::outgoing_message::OutgoingMessageSender;

pub(crate) struct CoreMessageProcessor {
    threads: ThreadManagerState,
    outgoing: Arc<OutgoingMessageSender>,
}

impl CoreMessageProcessor {
    pub fn new(outgoing: Arc<OutgoingMessageSender>) -> Self {
        Self {
            threads: ThreadManagerState::new(),
            outgoing,
        }
    }

    pub async fn process_request(
        &self,
        request: ClientRequest,
    ) -> Result<serde_json::Value, app_server_protocol::JSONRPCErrorError> {
        match request {
            ClientRequest::TurnStart { params, .. } => {
                let thread_id = params.thread_id.parse::<ThreadId>().map_err(|err| {
                    app_server_protocol::JSONRPCErrorError {
                        code: -32602,
                        data: None,
                        message: format!("invalid thread id: {err}"),
                    }
                })?;
                let message = self
                    .threads
                    .run_user_input(thread_id, params.input)
                    .await
                    .map_err(|err| app_server_protocol::JSONRPCErrorError {
                        code: -32000,
                        data: None,
                        message: err.to_string(),
                    })?;
                serde_json::to_value(TurnStartResponse {
                    thread_id: thread_id.to_string(),
                    message,
                })
                .map_err(|err| app_server_protocol::JSONRPCErrorError {
                    code: -32603,
                    data: None,
                    message: err.to_string(),
                })
            }
        }
    }
}
