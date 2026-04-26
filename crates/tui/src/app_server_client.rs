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
                                    let _ = request_sender
                                        .send(in_process::InProcessClientMessage::Request {
                                            request,
                                            response_tx,
                                        })
                                        .await;
                                });
                            }
                            None => {}
                        }
                    }
                    event = handle.next_event(), if event_stream_enabled => {
                        match event {
                            Some(event) => {
                                let _ = event_tx.send(event).await;
                            }
                            None => {
                                event_stream_enabled = false;
                            }
                        }
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

    pub(crate) async fn request(
        &self,
        request: ClientRequest,
    ) -> std::result::Result<serde_json::Value, JSONRPCErrorError> {
        let (response_tx, response_rx) = oneshot::channel();
        let command = ClientCommand::Request {
            request: Box::new(request),
            response_tx,
        };
        self.command_tx
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
}
