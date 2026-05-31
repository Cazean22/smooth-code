use std::sync::Arc;

use app_server_protocol::{ClientRequest, JsonRpcError};
use futures_util::FutureExt;
use tokio::sync::{mpsc, oneshot};

use crate::{
    core_message_processor::CoreMessageProcessor, error::AppServerResult,
    in_process::InProcessServerEvent, outgoing_message::OutgoingMessageSender,
};

pub(crate) struct MessageProcessor {
    core_message_processor: CoreMessageProcessor,
}

impl MessageProcessor {
    pub(crate) async fn new(
        event_tx: mpsc::Sender<InProcessServerEvent>,
        outgoing: Arc<OutgoingMessageSender>,
    ) -> AppServerResult<Self> {
        let core_message_processor = CoreMessageProcessor::new(event_tx, outgoing).await?;
        Ok(Self {
            core_message_processor,
        })
    }

    #[tracing::instrument(
        name = "app_server.process_client_request",
        skip(self, request, response_tx)
    )]
    pub(crate) async fn process_client_request(
        self: &Arc<Self>,
        request: ClientRequest,
        response_tx: oneshot::Sender<std::result::Result<serde_json::Value, JsonRpcError>>,
    ) {
        let result = self
            .core_message_processor
            .process_request(request)
            .boxed()
            .await;
        let _ = response_tx.send(result);
    }
}
