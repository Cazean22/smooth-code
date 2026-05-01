use app_server::in_process::{self, InProcessServerEvent, InProcessStartArgs};
use app_server_protocol::{ClientCommand, ClientRequest, JSONRPCErrorError};
use tokio::sync::{mpsc, oneshot};
use tracing::Instrument;

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
        let worker_handle = tokio::task::Builder::new()
            .name("tui.app_server.worker")
            .spawn(async move {
                let mut event_stream_enabled = true;
                loop {
                    tokio::select! {
                        command = command_rx.recv() => {
                            match command {
                                Some(ClientCommand::Request { request, response_tx }) => {
                                    let request_name = match request.as_ref() {
                                        ClientRequest::ThreadStart { .. } => "thread_start",
                                        ClientRequest::TurnStart { .. } => "turn_start",
                                        ClientRequest::ThreadResume { .. } => "thread_resume",
                                        ClientRequest::ThreadList { .. } => "thread_list",
                                    };
                                    let request_sender = request_sender.clone();
                                    let request_span = tracing::info_span!(
                                        "tui.app_server.forward_request",
                                        request = request_name,
                                    );

                                    // Request waits happen on a detached task so
                                    // this loop can keep draining runtime events
                                    // while the request is blocked on client input.
                                    tokio::task::Builder::new()
                                        .name("tui.app_server.forward_request")
                                        .spawn(
                                            async move {
                                                let _ = request_sender
                                                    .send(ClientCommand::Request {
                                                        request,
                                                        response_tx,
                                                    })
                                                    .await;
                                            }
                                            .instrument(request_span),
                                        )
                                        .expect("failed to spawn app-server request forwarder");
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
            })
            .expect("failed to spawn tui app-server worker");
        Ok(Self {
            command_tx,
            event_rx,
            worker_handle,
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

    pub(crate) async fn next_event(&mut self) -> Option<InProcessServerEvent> {
        self.event_rx.recv().await
    }
}
