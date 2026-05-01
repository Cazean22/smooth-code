use std::path::PathBuf;

use rig::{completion::ToolDefinition, tool::Tool};
use serde::Deserialize;

use crate::{
    ToolFailure,
    shared::{resolve_path, truncate_output},
};

#[derive(Clone)]
pub struct ListDirTool {
    cwd: PathBuf,
}

impl ListDirTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self { cwd }
    }
}

#[derive(Deserialize)]
pub struct ListDirArgs {
    path: Option<String>,
}

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
            .map_err(|err| ToolFailure::new(format!("failed to read {}: {err}", path.display())))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|err| ToolFailure::new(format!("failed to read {}: {err}", path.display())))?;
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
