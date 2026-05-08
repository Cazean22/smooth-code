use std::sync::Arc;

use app_server_protocol::DynamicToolCallParams;
use rig::{completion::ToolDefinition, tool::Tool};
use serde::Deserialize;
use tokio::sync::RwLock;
use uuid::Uuid;

use crate::{DynamicToolClient, ToolFailure};

#[derive(Deserialize)]
pub struct DynamicToolArgs {
    payload: String,
}

#[derive(Clone)]
pub struct DynamicTool {
    name: String,
    thread_id: smooth_protocol::ThreadId,
    client: Arc<dyn DynamicToolClient>,
    current_turn_id: Arc<RwLock<Option<String>>>,
}

impl DynamicTool {
    pub fn new(
        name: impl Into<String>,
        thread_id: smooth_protocol::ThreadId,
        client: Arc<dyn DynamicToolClient>,
        current_turn_id: Arc<RwLock<Option<String>>>,
    ) -> Self {
        Self {
            name: name.into(),
            thread_id,
            client,
            current_turn_id,
        }
    }
}

impl Tool for DynamicTool {
    const NAME: &'static str = "dynamic_tool";

    type Error = ToolFailure;
    type Args = DynamicToolArgs;
    type Output = String;

    fn name(&self) -> String {
        self.name.clone()
    }

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: self.name.clone(),
            description: "Dispatch a dynamic tool call to the in-process client. Provide arguments as a JSON-encoded string in the `payload` field.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "payload": {
                        "type": "string",
                        "description": "JSON-encoded arguments to forward to the in-process client."
                    }
                },
                "required": ["payload"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let turn_id = self
            .current_turn_id
            .read()
            .await
            .clone()
            .ok_or_else(|| ToolFailure::new("no active turn id"))?;
        let arguments = serde_json::from_str(&args.payload)
            .map_err(|err| ToolFailure::new(format!("invalid payload JSON: {err}")))?;
        let params = DynamicToolCallParams {
            thread_id: self.thread_id.to_string(),
            turn_id,
            call_id: Uuid::new_v4().to_string(),
            tool: self.name.clone(),
            arguments,
        };

        let value = self
            .client
            .call(params)
            .await
            .map_err(|err| ToolFailure::new(err.message))?;
        Ok(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use app_server_protocol::JSONRPCErrorError;
    use futures_util::future::BoxFuture;

    use super::*;

    struct StubDynamicToolClient {
        last_params: Mutex<Option<DynamicToolCallParams>>,
        result: serde_json::Value,
    }

    impl DynamicToolClient for StubDynamicToolClient {
        fn call(
            &self,
            params: DynamicToolCallParams,
        ) -> BoxFuture<'static, Result<serde_json::Value, JSONRPCErrorError>> {
            *self
                .last_params
                .lock()
                .expect("stub params mutex should lock") = Some(params);
            let result = self.result.clone();
            Box::pin(async move { Ok(result) })
        }

        fn abort_pending_server_requests(&self) -> BoxFuture<'static, ()> {
            Box::pin(async {})
        }
    }

    #[tokio::test]
    async fn dynamic_tool_uses_runtime_tool_name_and_current_turn_id() {
        let stub = Arc::new(StubDynamicToolClient {
            last_params: Mutex::new(None),
            result: serde_json::json!({ "ok": true }),
        });
        let current_turn_id = Arc::new(RwLock::new(Some("turn-42".to_string())));
        let tool = DynamicTool::new(
            "dynamic_echo",
            smooth_protocol::ThreadId::new(),
            stub.clone(),
            current_turn_id,
        );

        let definition = tool.definition(String::new()).await;
        let output = tool
            .call(DynamicToolArgs {
                payload: "{\"message\":\"hello\"}".to_string(),
            })
            .await
            .expect("tool call should succeed");
        let params = stub
            .last_params
            .lock()
            .expect("stub params mutex should lock")
            .clone()
            .expect("tool call should record params");

        assert_eq!(tool.name(), "dynamic_echo");
        assert_eq!(definition.name, "dynamic_echo");
        assert_eq!(params.turn_id, "turn-42");
        assert_eq!(params.tool, "dynamic_echo");
        assert_eq!(params.arguments, serde_json::json!({ "message": "hello" }));
        assert_eq!(output, "{\"ok\":true}");
    }

    #[tokio::test]
    async fn dynamic_tool_fails_without_an_active_turn() {
        let current_turn_id = Arc::new(RwLock::new(None));
        let tool = DynamicTool::new(
            "dynamic_echo",
            smooth_protocol::ThreadId::new(),
            Arc::new(StubDynamicToolClient {
                last_params: Mutex::new(None),
                result: serde_json::json!({ "ok": true }),
            }),
            current_turn_id,
        );

        let err = tool
            .call(DynamicToolArgs {
                payload: "{}".to_string(),
            })
            .await
            .expect_err("tool call should fail without an active turn");

        assert_eq!(err.to_string(), "no active turn id");
    }

    #[tokio::test]
    async fn dynamic_tool_fails_on_invalid_payload_json() {
        let current_turn_id = Arc::new(RwLock::new(Some("turn-42".to_string())));
        let tool = DynamicTool::new(
            "dynamic_echo",
            smooth_protocol::ThreadId::new(),
            Arc::new(StubDynamicToolClient {
                last_params: Mutex::new(None),
                result: serde_json::json!({ "ok": true }),
            }),
            current_turn_id,
        );

        let err = tool
            .call(DynamicToolArgs {
                payload: "{not-json}".to_string(),
            })
            .await
            .expect_err("tool call should fail on invalid payload JSON");

        assert!(err.to_string().starts_with("invalid payload JSON: "));
    }
}
