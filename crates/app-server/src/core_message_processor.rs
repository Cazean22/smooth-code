use std::sync::Arc;

use app_server_protocol::ClientRequest;
use smooth_core::ThreadManagerState;

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

    pub async fn process_request(&self, request: ClientRequest) {}
}
