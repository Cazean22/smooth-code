use std::path::PathBuf;
use std::process::Command;

use rig::{
    completion::ToolDefinition,
    tool::Tool,
};
use serde::Deserialize;

const MAX_TOOL_OUTPUT_BYTES: usize = 16 * 1024;

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
