use std::{
    collections::HashSet,
    sync::{Arc, OnceLock, atomic::AtomicBool},
};

use app_server_protocol::{ClientRequest, JSONRPCErrorError};
use futures_util::FutureExt;
use tokio::sync::{mpsc, oneshot};

use crate::{core_message_processor::CoreMessageProcessor, in_process::InProcessServerEvent};

#[derive(Debug, Default)]
pub(crate) struct ConnectionSessionState {
    initialized: OnceLock<InitializedConnectionSessionState>,
}

#[derive(Debug)]
struct InitializedConnectionSessionState {
    opted_out_notification_methods: HashSet<String>,
    app_server_client_name: String,
    client_version: String,
}

pub(crate) struct MessageProcessor {
    core_message_processor: CoreMessageProcessor,
}

impl MessageProcessor {
    pub(crate) fn new(event_tx: mpsc::Sender<InProcessServerEvent>) -> Self {
        let core_message_processor = CoreMessageProcessor::new(event_tx);
        Self { core_message_processor }
    }
    pub(crate) async fn process_client_request(
        self: &Arc<Self>,
        request: ClientRequest,
        session: Arc<ConnectionSessionState>,
        outbound_initialized: &AtomicBool,
        response_tx: oneshot::Sender<std::result::Result<serde_json::Value, JSONRPCErrorError>>,
    ) {
        let _ = (session, outbound_initialized);
        let result = self
            .core_message_processor
            .process_request(request)
            .boxed()
            .await;
        let _ = response_tx.send(result);
    }
}
