use std::{fs, io::ErrorKind, path::PathBuf};

use rig::{completion::ToolDefinition, tool::Tool};
use schemars::{JsonSchema, schema_for};
use serde::Deserialize;
use smooth_protocol::{FileChange, FileChangeOperation, FileChangeOutput};

use crate::{
    MAX_FILE_CHANGE_BYTES, ToolFailure, encode_tool_output, shared::resolve_path_for_write,
};

const DESCRIPTION: &str = r#"Delete an existing file from the local filesystem.

Usage:
- `file_path` may be an absolute path, or a path relative to the current working directory, but must resolve inside the workspace.
- This tool deletes files only. It rejects missing paths and directories.
- Use this tool for source file removals so the UI can render a structured deletion diff."#;

#[derive(Clone)]
pub struct DeleteTool {
    cwd: PathBuf,
}

impl DeleteTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self { cwd }
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DeleteArgs {
    /// Absolute path to the file, or a path relative to the current working directory.
    file_path: String,
}

impl Tool for DeleteTool {
    const NAME: &'static str = "delete";

    type Error = ToolFailure;
    type Args = DeleteArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: DESCRIPTION.to_string(),
            parameters: schema_for!(DeleteArgs).to_value(),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let path = resolve_path_for_write(&self.cwd, &args.file_path)?;
        let metadata = fs::metadata(&path).map_err(|err| {
            if err.kind() == ErrorKind::NotFound {
                ToolFailure::new(format!("file {} does not exist", path.display()))
            } else {
                ToolFailure::new(format!("failed to inspect {}: {err}", path.display()))
            }
        })?;
        if metadata.is_dir() {
            return Err(ToolFailure::new(format!(
                "{} is a directory; delete only removes files",
                path.display()
            )));
        }

        let previous_content = match fs::read_to_string(&path) {
            Ok(content) => PreviousContent::Utf8(content),
            Err(err) => PreviousContent::Unreadable(err.to_string()),
        };
        let bytes = metadata.len() as usize;

        fs::remove_file(&path).map_err(|err| {
            ToolFailure::new(format!("failed to delete {}: {err}", path.display()))
        })?;

        let change = match previous_content {
            PreviousContent::Utf8(content) => {
                if content.len() > MAX_FILE_CHANGE_BYTES {
                    FileChange::Omitted {
                        operation: FileChangeOperation::Delete,
                        reason: format!(
                            "deleted file content omitted because it exceeds {} bytes",
                            MAX_FILE_CHANGE_BYTES
                        ),
                        added: 0,
                        removed: content.lines().count(),
                        bytes: content.len(),
                    }
                } else {
                    FileChange::Delete { content }
                }
            }
            PreviousContent::Unreadable(reason) => FileChange::Omitted {
                operation: FileChangeOperation::Delete,
                reason: format!("deleted file content unavailable: {reason}"),
                added: 0,
                removed: 0,
                bytes,
            },
        };
        let model_output = format!("deleted {} ({} bytes)", path.display(), bytes);
        Ok(encode_tool_output(
            model_output,
            Some(FileChangeOutput { path, change }),
        ))
    }
}

enum PreviousContent {
    Utf8(String),
    Unreadable(String),
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use crate::decode_tool_output_for_tool;

    use super::*;

    fn fixture() -> (DeleteTool, TempDir) {
        let tmp = TempDir::new().expect("create tempdir");
        let tool = DeleteTool::new(tmp.path().to_path_buf());
        (tool, tmp)
    }

    fn args(file_path: impl Into<String>) -> DeleteArgs {
        DeleteArgs {
            file_path: file_path.into(),
        }
    }

    #[tokio::test]
    async fn deletes_existing_utf8_file() {
        let (tool, tmp) = fixture();
        let path = tmp.path().join("foo.txt");
        fs::write(&path, "hello\nworld\n").unwrap();

        let output = tool
            .call(args("foo.txt"))
            .await
            .expect("delete should succeed");

        let resolved_path = fs::canonicalize(tmp.path()).unwrap().join("foo.txt");
        assert!(!path.exists());
        let decoded = decode_tool_output_for_tool("delete", output, true);
        assert_eq!(
            decoded.model_output,
            format!("deleted {} (12 bytes)", resolved_path.display())
        );
        let file_change = decoded.file_change.expect("file change metadata");
        assert_eq!(file_change.path, resolved_path);
        assert_eq!(
            file_change.change,
            FileChange::Delete {
                content: "hello\nworld\n".to_string(),
            }
        );
    }

    #[tokio::test]
    async fn errors_when_file_is_missing() {
        let (tool, _tmp) = fixture();

        let err = tool
            .call(args("missing.txt"))
            .await
            .expect_err("delete should fail");

        assert!(err.to_string().contains("does not exist"));
    }

    #[tokio::test]
    async fn errors_when_path_is_directory() {
        let (tool, tmp) = fixture();
        fs::create_dir(tmp.path().join("dir")).unwrap();

        let err = tool
            .call(args("dir"))
            .await
            .expect_err("delete should fail");

        assert!(err.to_string().contains("is a directory"));
    }

    #[tokio::test]
    async fn rejects_path_outside_workspace() {
        let (tool, _tmp) = fixture();
        let other = TempDir::new().expect("create second tempdir");
        let path = other.path().join("file.txt");
        fs::write(&path, "hello\n").unwrap();

        let err = tool
            .call(args(path.display().to_string()))
            .await
            .expect_err("delete should fail");

        assert!(err.to_string().contains("escapes the workspace"));
        assert!(path.exists());
    }

    #[tokio::test]
    async fn non_utf8_file_emits_omitted_delete_metadata() {
        let (tool, tmp) = fixture();
        let path = tmp.path().join("binary.dat");
        fs::write(&path, [0xff, 0xfe]).unwrap();

        let output = tool
            .call(args("binary.dat"))
            .await
            .expect("delete should succeed");

        assert!(!path.exists());
        let decoded = decode_tool_output_for_tool("delete", output, true);
        match decoded.file_change.expect("file change metadata").change {
            FileChange::Omitted {
                operation,
                reason,
                added,
                removed,
                bytes,
            } => {
                assert_eq!(operation, FileChangeOperation::Delete);
                assert!(reason.contains("deleted file content unavailable"));
                assert_eq!(added, 0);
                assert_eq!(removed, 0);
                assert_eq!(bytes, 2);
            }
            _ => panic!("expected omitted file change"),
        }
    }
}
