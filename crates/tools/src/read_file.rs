use std::path::PathBuf;

use rig::{completion::ToolDefinition, tool::Tool};
use serde::Deserialize;

use crate::{
    ToolFailure,
    shared::{resolve_path, truncate_output},
};

#[derive(Clone)]
pub struct ReadFileTool {
    cwd: PathBuf,
}

impl ReadFileTool {
    pub fn new(cwd: PathBuf) -> Self {
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
            .map_err(|err| ToolFailure::new(format!("failed to read {}: {err}", path.display())))?;
        Ok(truncate_output(content))
    }
}
