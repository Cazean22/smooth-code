use std::path::PathBuf;
use std::process::Command;

use rig::{completion::ToolDefinition, tool::Tool};
use serde::Deserialize;

use crate::{ToolFailure, shared::truncate_output};

#[derive(Clone)]
pub struct RunCommandTool {
    cwd: PathBuf,
}

impl RunCommandTool {
    pub fn new(cwd: PathBuf) -> Self {
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
            description: "Run a shell command inside the current workspace and return combined stdout/stderr. Use this for inspection, validation, formatters, and project commands; use structured file tools such as edit, write, and delete for source changes instead of shell rewrite scripts.".to_string(),
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
            .map_err(|err| ToolFailure::new(format!("failed to run command: {err}")))?;

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
