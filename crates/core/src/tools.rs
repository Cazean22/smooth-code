use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

use app_server_protocol::{DynamicToolCallParams, JSONRPCErrorError};
use futures_util::future::BoxFuture;
use rig::{completion::ToolDefinition, tool::Tool};
use serde::Deserialize;
use tokio::sync::watch;
use uuid::Uuid;

const MAX_TOOL_OUTPUT_BYTES: usize = 16 * 1024;

pub trait DynamicToolClient: Send + Sync {
    fn call(
        &self,
        params: DynamicToolCallParams,
    ) -> BoxFuture<'static, Result<serde_json::Value, JSONRPCErrorError>>;

    fn abort_pending_server_requests(&self) -> BoxFuture<'static, ()>;
}

pub trait DynamicToolClientFactory: Send + Sync {
    fn build(&self, thread_id: smooth_protocol::ThreadId) -> Arc<dyn DynamicToolClient>;
}

#[derive(Clone)]
pub(crate) struct ListDirTool {
    cwd: PathBuf,
}

impl ListDirTool {
    pub(crate) fn new(cwd: PathBuf) -> Self {
        Self { cwd }
    }
}

#[derive(Deserialize)]
pub struct ListDirArgs {
    path: Option<String>,
}

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ToolFailure(String);

impl Tool for ListDirTool {
    const NAME: &'static str = "list_dir";

    type Error = ToolFailure;
    type Args = ListDirArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "List files and directories relative to the current workspace."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Optional relative path to list. Defaults to the workspace root."
                    }
                }
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let path = resolve_path(&self.cwd, args.path.as_deref())?;
        let mut entries = std::fs::read_dir(&path)
            .map_err(|err| ToolFailure(format!("failed to read {}: {err}", path.display())))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|err| ToolFailure(format!("failed to read {}: {err}", path.display())))?;
        entries.sort_by_key(|entry| entry.file_name());

        let output = entries
            .into_iter()
            .map(|entry| {
                let file_type = entry.file_type();
                let suffix = match file_type {
                    Ok(file_type) if file_type.is_dir() => "/",
                    Ok(file_type) if file_type.is_symlink() => "@",
                    _ => "",
                };
                format!("{}{}", entry.file_name().to_string_lossy(), suffix)
            })
            .collect::<Vec<_>>()
            .join("\n");
        Ok(truncate_output(output))
    }
}

#[derive(Clone)]
pub(crate) struct ReadFileTool {
    cwd: PathBuf,
}

impl ReadFileTool {
    pub(crate) fn new(cwd: PathBuf) -> Self {
        Self { cwd }
    }
}

#[derive(Deserialize)]
pub struct ReadFileArgs {
    path: String,
}

impl Tool for ReadFileTool {
    const NAME: &'static str = "read_file";

    type Error = ToolFailure;
    type Args = ReadFileArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Read a UTF-8 text file relative to the current workspace.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Relative path to the file to read."
                    }
                },
                "required": ["path"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let path = resolve_path(&self.cwd, Some(&args.path))?;
        let content = std::fs::read_to_string(&path)
            .map_err(|err| ToolFailure(format!("failed to read {}: {err}", path.display())))?;
        Ok(truncate_output(content))
    }
}

#[derive(Clone)]
pub(crate) struct RunCommandTool {
    cwd: PathBuf,
}

impl RunCommandTool {
    pub(crate) fn new(cwd: PathBuf) -> Self {
        Self { cwd }
    }
}

#[derive(Deserialize)]
pub struct RunCommandArgs {
    command: String,
}

impl Tool for RunCommandTool {
    const NAME: &'static str = "run_command";

    type Error = ToolFailure;
    type Args = RunCommandArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: "Run a shell command inside the current workspace and return combined stdout/stderr.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Shell command to execute."
                    }
                },
                "required": ["command"]
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let output = Command::new("zsh")
            .arg("-lc")
            .arg(&args.command)
            .current_dir(&self.cwd)
            .output()
            .map_err(|err| ToolFailure(format!("failed to run command: {err}")))?;

        let mut text = String::new();
        if !output.stdout.is_empty() {
            text.push_str(&String::from_utf8_lossy(&output.stdout));
        }
        if !output.stderr.is_empty() {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(&String::from_utf8_lossy(&output.stderr));
        }
        if !output.status.success() {
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text.push_str(&format!("command exited with status {}", output.status));
        }

        Ok(truncate_output(text))
    }
}

#[derive(Clone)]
pub(crate) struct DynamicTool {
    name: String,
    thread_id: smooth_protocol::ThreadId,
    client: Arc<dyn DynamicToolClient>,
    current_turn_id: Arc<watch::Sender<Option<String>>>,
}

impl DynamicTool {
    pub(crate) fn new(
        name: impl Into<String>,
        thread_id: smooth_protocol::ThreadId,
        client: Arc<dyn DynamicToolClient>,
        current_turn_id: Arc<watch::Sender<Option<String>>>,
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
    type Args = serde_json::Value;
    type Output = String;

    fn name(&self) -> String {
        self.name.clone()
    }

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: self.name.clone(),
            description: "Dispatch a dynamic tool call to the in-process client.".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "additionalProperties": true
            }),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let turn_id = self
            .current_turn_id
            .borrow()
            .clone()
            .ok_or_else(|| ToolFailure("no active turn id".to_string()))?;
        let params = DynamicToolCallParams {
            thread_id: self.thread_id.to_string(),
            turn_id: turn_id.clone(),
            call_id: Uuid::new_v4().to_string(),
            tool: self.name.clone(),
            arguments: args,
        };

        let value = self
            .client
            .call(params)
            .await
            .map_err(|err| ToolFailure(err.message))?;
        Ok(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use tokio::sync::watch;

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
        let (current_turn_id, _) = watch::channel(Some("turn-42".to_string()));
        let tool = DynamicTool::new(
            "dynamic_echo",
            smooth_protocol::ThreadId::new(),
            stub.clone(),
            Arc::new(current_turn_id),
        );

        let definition = tool.definition(String::new()).await;
        let output = tool
            .call(serde_json::json!({ "message": "hello" }))
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
        let (current_turn_id, _) = watch::channel(None);
        let tool = DynamicTool::new(
            "dynamic_echo",
            smooth_protocol::ThreadId::new(),
            Arc::new(StubDynamicToolClient {
                last_params: Mutex::new(None),
                result: serde_json::json!({ "ok": true }),
            }),
            Arc::new(current_turn_id),
        );

        let err = tool
            .call(serde_json::json!({}))
            .await
            .expect_err("tool call should fail without an active turn");

        assert_eq!(err.to_string(), "no active turn id");
    }
}

fn resolve_path(cwd: &PathBuf, path: Option<&str>) -> Result<PathBuf, ToolFailure> {
    let path = match path {
        Some(path) if !path.is_empty() => cwd.join(path),
        _ => cwd.clone(),
    };
    let canonical = path
        .canonicalize()
        .map_err(|err| ToolFailure(format!("failed to resolve {}: {err}", path.display())))?;
    if !canonical.starts_with(cwd) {
        return Err(ToolFailure(format!(
            "path {} escapes the workspace",
            canonical.display()
        )));
    }
    Ok(canonical)
}

fn truncate_output(mut output: String) -> String {
    if output.len() <= MAX_TOOL_OUTPUT_BYTES {
        return output;
    }
    output.truncate(MAX_TOOL_OUTPUT_BYTES);
    output.push_str("\n...[truncated]");
    output
}
