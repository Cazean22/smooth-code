use std::{fs, path::PathBuf};

use rig::{completion::ToolDefinition, tool::Tool};
use schemars::{JsonSchema, schema_for};
use serde::Deserialize;
use smooth_protocol::{FileChange, FileChangeOutput};

use crate::{ToolFailure, encode_tool_output, shared::resolve_path_for_write};

const DESCRIPTION: &str = r#"Write a UTF-8 text file to the local filesystem.

Usage:
- `file_path` may be an absolute path, or a path relative to the current working directory, but must resolve inside the workspace.
- If a file already exists at that path, it is overwritten.
- The parent directory must already exist; this tool does not create missing directories.
- Prefer using `read` plus a future edit tool to modify existing files; use `write` for new files or full rewrites."#;

#[derive(Clone)]
pub struct WriteTool {
    cwd: PathBuf,
}

impl WriteTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self { cwd }
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WriteArgs {
    /// Absolute path to the file, or a path relative to the current working directory.
    file_path: String,

    /// UTF-8 content to write to the file. Existing files at this path are overwritten.
    content: String,
}

impl Tool for WriteTool {
    const NAME: &'static str = "write";

    type Error = ToolFailure;
    type Args = WriteArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: DESCRIPTION.to_string(),
            parameters: schema_for!(WriteArgs).to_value(),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let path = resolve_path_for_write(&self.cwd, &args.file_path)?;
        let previous_content = fs::read_to_string(&path).ok();
        let bytes = args.content.len();
        fs::write(&path, &args.content).map_err(|err| {
            ToolFailure::new(format!("failed to write {}: {err}", path.display()))
        })?;
        let model_output = format!("wrote {bytes} bytes to {}", path.display());
        let change = match previous_content {
            Some(previous_content) => FileChange::Update {
                unified_diff: diffy::create_patch(&previous_content, &args.content).to_string(),
                move_path: None,
            },
            None => FileChange::Add {
                content: args.content,
            },
        };
        Ok(encode_tool_output(
            model_output,
            Some(FileChangeOutput { path, change }),
        ))
    }
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use crate::decode_tool_output;

    use super::*;

    fn fixture() -> (WriteTool, TempDir) {
        let tmp = TempDir::new().expect("create tempdir");
        let tool = WriteTool::new(tmp.path().to_path_buf());
        (tool, tmp)
    }

    fn args(file_path: impl Into<String>, content: impl Into<String>) -> WriteArgs {
        WriteArgs {
            file_path: file_path.into(),
            content: content.into(),
        }
    }

    #[tokio::test]
    async fn writes_new_file() {
        let (tool, tmp) = fixture();

        let output = tool
            .call(args("foo.txt", "hello"))
            .await
            .expect("write should succeed");

        let path = tmp.path().join("foo.txt");
        let resolved_path = fs::canonicalize(tmp.path()).unwrap().join("foo.txt");
        assert_eq!(fs::read_to_string(&path).unwrap(), "hello");
        let decoded = decode_tool_output(output);
        assert_eq!(
            decoded.model_output,
            format!("wrote 5 bytes to {}", resolved_path.display())
        );
        let file_change = decoded.file_change.expect("file change metadata");
        assert_eq!(file_change.path, resolved_path);
        assert_eq!(
            file_change.change,
            FileChange::Add {
                content: "hello".to_string(),
            }
        );
    }

    #[tokio::test]
    async fn overwrites_existing_file() {
        let (tool, tmp) = fixture();
        let path = tmp.path().join("foo.txt");
        fs::write(&path, "before").unwrap();

        let output = tool
            .call(args("foo.txt", "after"))
            .await
            .expect("write should succeed");

        assert_eq!(fs::read_to_string(path).unwrap(), "after");
        let decoded = decode_tool_output(output);
        match decoded.file_change.expect("file change metadata").change {
            FileChange::Update { unified_diff, .. } => {
                assert!(unified_diff.contains("-before"));
                assert!(unified_diff.contains("+after"));
            }
            _ => panic!("expected update file change"),
        }
    }

    #[tokio::test]
    async fn resolves_relative_path_against_cwd() {
        let (tool, tmp) = fixture();
        fs::create_dir(tmp.path().join("sub")).unwrap();

        tool.call(args("sub/file.txt", "hello"))
            .await
            .expect("write should succeed");

        assert_eq!(
            fs::read_to_string(tmp.path().join("sub/file.txt")).unwrap(),
            "hello"
        );
    }

    #[tokio::test]
    async fn accepts_absolute_path_inside_cwd() {
        let (tool, tmp) = fixture();
        let path = tmp.path().join("a.txt");

        tool.call(args(path.display().to_string(), "hi"))
            .await
            .expect("write should succeed");

        assert_eq!(fs::read_to_string(path).unwrap(), "hi");
    }

    #[tokio::test]
    async fn rejects_path_outside_cwd() {
        let (tool, _tmp) = fixture();
        let other = TempDir::new().expect("create second tempdir");
        let path = other.path().join("file.txt");

        let err = tool
            .call(args(path.display().to_string(), "nope"))
            .await
            .expect_err("write should fail");

        assert!(err.to_string().contains("escapes the workspace"));
    }

    #[tokio::test]
    async fn errors_when_parent_missing() {
        let (tool, _tmp) = fixture();

        let err = tool
            .call(args("nope/x.txt", "hello"))
            .await
            .expect_err("write should fail");

        assert!(err.to_string().contains("failed to resolve"));
    }

    #[tokio::test]
    async fn writes_empty_content() {
        let (tool, tmp) = fixture();

        let output = tool
            .call(args("empty.txt", ""))
            .await
            .expect("write should succeed");

        let path = tmp.path().join("empty.txt");
        let resolved_path = fs::canonicalize(tmp.path()).unwrap().join("empty.txt");
        assert_eq!(fs::read_to_string(&path).unwrap(), "");
        let decoded = decode_tool_output(output);
        assert_eq!(
            decoded.model_output,
            format!("wrote 0 bytes to {}", resolved_path.display())
        );
        assert_eq!(
            decoded.file_change.expect("file change metadata").change,
            FileChange::Add {
                content: String::new(),
            }
        );
    }
}
