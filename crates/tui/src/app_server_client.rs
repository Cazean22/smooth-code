use app_server::in_process::{self, InProcessServerEvent, InProcessStartArgs};
use app_server_protocol::{ClientRequest, JSONRPCErrorError};
use tokio::sync::{mpsc, oneshot};

enum ClientCommand {
    Request {
        request: Box<ClientRequest>,
        response_tx: oneshot::Sender<std::result::Result<serde_json::Value, JSONRPCErrorError>>,
    },
}
pub(crate) struct AppServerClient {
    command_tx: mpsc::Sender<ClientCommand>,
    event_rx: mpsc::Receiver<InProcessServerEvent>,
    worker_handle: tokio::task::JoinHandle<()>,
}

impl AppServerClient {
    pub(crate) fn start(channel_capacity: usize) -> anyhow::Result<Self> {
        let mut handle = in_process::start(InProcessStartArgs { channel_capacity });
        let request_sender = handle.client_tx.clone();
        let (command_tx, mut command_rx) = mpsc::channel::<ClientCommand>(channel_capacity);
        let (event_tx, event_rx) = mpsc::channel::<InProcessServerEvent>(channel_capacity);
        let worker_handle = tokio::spawn(async move {
            let mut event_stream_enabled = true;
            let mut skipped_events = 0usize;
            loop {
                tokio::select! {
                    command = command_rx.recv() => {
                        match command {
                            Some(ClientCommand::Request { request, response_tx }) => {
                                let request_sender = request_sender.clone();
                                // Request waits happen on a detached task so
                                // this loop can keep draining runtime events
                                // while the request is blocked on client input.
                                tokio::spawn(async move {
                                    // let result = request_sender.send(*request).await;
                                    // let _ = response_tx.send(result);
                                });
                            }
                            None => {}
                        }
                    }
                    event = handle.next_event(), if event_stream_enabled => {
                        todo!()
                    }
                }
            }
        });
        Ok(Self {
            command_tx,
            event_rx,
            worker_handle,
        })
    }
}
