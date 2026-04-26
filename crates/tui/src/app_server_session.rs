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
}
