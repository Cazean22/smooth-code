use std::{fs, io::ErrorKind, path::PathBuf};

use rig::{completion::ToolDefinition, tool::Tool};
use schemars::{JsonSchema, schema_for};
use serde::Deserialize;
use smooth_protocol::{FileChange, FileChangeOperation, FileChangeOutput};

use crate::{MAX_FILE_CHANGE_BYTES, ToolError, encode_tool_output, shared::resolve_path_for_write};

const DESCRIPTION: &str = r#"Write a UTF-8 text file to the local filesystem.

Usage:
- `file_path` may be an absolute path, or a path relative to the current working directory, but must resolve inside the workspace.
- If a file already exists at that path, it is overwritten.
- The parent directory must already exist; this tool does not create missing directories.
- Prefer using `read` plus `edit` to modify existing files, `delete` to remove files, and `write` for new files or intentional full rewrites."#;

#[derive(Clone)]
pub struct WriteTool {
    cwd: PathBuf,
    max_file_change_bytes: usize,
}

impl WriteTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            max_file_change_bytes: MAX_FILE_CHANGE_BYTES,
        }
    }

    /// Override the file-change byte cap (from the resolved app config).
    pub fn with_max_file_change_bytes(mut self, max_file_change_bytes: usize) -> Self {
        self.max_file_change_bytes = max_file_change_bytes;
        self
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

    type Error = ToolError;
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
        let previous_content = match fs::metadata(&path) {
            Ok(_) => match fs::read_to_string(&path) {
                Ok(content) => PreviousContent::Utf8(content),
                Err(err) => PreviousContent::Unreadable(err.to_string()),
            },
            Err(err) if err.kind() == ErrorKind::NotFound => PreviousContent::Missing,
            Err(err) => PreviousContent::Unreadable(err.to_string()),
        };
        let bytes = args.content.len();
        fs::write(&path, &args.content)
            .map_err(|err| ToolError::io(format!("failed to write {}: {err}", path.display())))?;
        let model_output = format!("wrote {bytes} bytes to {}", path.display());
        let change = match previous_content {
            PreviousContent::Utf8(previous_content) => {
                let unified_diff =
                    diffy::create_patch(&previous_content, &args.content).to_string();
                let (added, removed) = diff_line_counts(&unified_diff);
                if unified_diff.len() > self.max_file_change_bytes {
                    FileChange::Omitted {
                        operation: FileChangeOperation::Update,
                        reason: format!(
                            "diff omitted because it exceeds {} bytes",
                            self.max_file_change_bytes
                        ),
                        added,
                        removed,
                        bytes: unified_diff.len(),
                    }
                } else {
                    FileChange::Update {
                        unified_diff,
                        move_path: None,
                    }
                }
            }
            PreviousContent::Missing => {
                if args.content.len() > self.max_file_change_bytes {
                    FileChange::Omitted {
                        operation: FileChangeOperation::Add,
                        reason: format!(
                            "new file content omitted because it exceeds {} bytes",
                            self.max_file_change_bytes
                        ),
                        added: args.content.lines().count(),
                        removed: 0,
                        bytes: args.content.len(),
                    }
                } else {
                    FileChange::Add {
                        content: args.content,
                    }
                }
            }
            PreviousContent::Unreadable(reason) => FileChange::Omitted {
                operation: FileChangeOperation::Update,
                reason: format!("previous file content unavailable: {reason}"),
                added: args.content.lines().count(),
                removed: 0,
                bytes: args.content.len(),
            },
        };
        Ok(encode_tool_output(
            model_output,
            Some(FileChangeOutput { path, change }),
        ))
    }
}

enum PreviousContent {
    Missing,
    Utf8(String),
    Unreadable(String),
}

fn diff_line_counts(unified_diff: &str) -> (usize, usize) {
    diffy::Patch::from_str(unified_diff)
        .map(|patch| {
            patch.hunks().iter().flat_map(diffy::Hunk::lines).fold(
                (0, 0),
                |(added, removed), line| match line {
                    diffy::Line::Insert(_) => (added + 1, removed),
                    diffy::Line::Delete(_) => (added, removed + 1),
                    diffy::Line::Context(_) => (added, removed),
                },
            )
        })
        .unwrap_or((0, 0))
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use crate::decode_tool_output_for_tool;

    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn fixture() -> Result<(WriteTool, TempDir), std::io::Error> {
        let tmp = TempDir::new()?;
        let tool = WriteTool::new(tmp.path().to_path_buf());
        Ok((tool, tmp))
    }

    fn args(file_path: impl Into<String>, content: impl Into<String>) -> WriteArgs {
        WriteArgs {
            file_path: file_path.into(),
            content: content.into(),
        }
    }

    #[tokio::test]
    async fn writes_new_file() -> TestResult {
        let (tool, tmp) = fixture()?;

        let output = tool.call(args("foo.txt", "hello")).await?;

        let path = tmp.path().join("foo.txt");
        let resolved_path = fs::canonicalize(tmp.path())?.join("foo.txt");
        assert_eq!(fs::read_to_string(&path)?, "hello");
        let decoded = decode_tool_output_for_tool("write", output, true);
        assert_eq!(
            decoded.model_output,
            format!("wrote 5 bytes to {}", resolved_path.display())
        );
        let file_change = decoded
            .file_change
            .ok_or_else(|| std::io::Error::other("file change metadata"))?;
        assert_eq!(file_change.path, resolved_path);
        assert_eq!(
            file_change.change,
            FileChange::Add {
                content: "hello".to_string(),
            }
        );
        Ok(())
    }

    #[tokio::test]
    async fn overwrites_existing_file() -> TestResult {
        let (tool, tmp) = fixture()?;
        let path = tmp.path().join("foo.txt");
        fs::write(&path, "before")?;

        let output = tool.call(args("foo.txt", "after")).await?;

        assert_eq!(fs::read_to_string(path)?, "after");
        let decoded = decode_tool_output_for_tool("write", output, true);
        let file_change = decoded
            .file_change
            .ok_or_else(|| std::io::Error::other("file change metadata"))?;
        match file_change.change {
            FileChange::Update { unified_diff, .. } => {
                assert!(unified_diff.contains("-before"));
                assert!(unified_diff.contains("+after"));
            }
            _ => panic!("expected update file change"),
        }
        Ok(())
    }

    #[tokio::test]
    async fn large_new_file_omits_file_change_content() -> TestResult {
        let (tool, _tmp) = fixture()?;
        let large_content = "x".repeat(MAX_FILE_CHANGE_BYTES + 1);

        let output = tool.call(args("large.txt", large_content.clone())).await?;

        let decoded = decode_tool_output_for_tool("write", output, true);
        let file_change = decoded
            .file_change
            .ok_or_else(|| std::io::Error::other("file change metadata"))?;
        match file_change.change {
            FileChange::Omitted {
                operation,
                reason,
                added,
                removed,
                bytes,
            } => {
                assert_eq!(operation, FileChangeOperation::Add);
                assert!(reason.contains("new file content omitted"));
                assert_eq!(added, 1);
                assert_eq!(removed, 0);
                assert_eq!(bytes, large_content.len());
            }
            _ => panic!("expected omitted file change"),
        }
        Ok(())
    }

    #[tokio::test]
    async fn non_utf8_existing_file_is_not_reported_as_added() -> TestResult {
        let (tool, tmp) = fixture()?;
        let path = tmp.path().join("binary.dat");
        fs::write(&path, [0xff, 0xfe])?;

        let output = tool.call(args("binary.dat", "replacement")).await?;

        let decoded = decode_tool_output_for_tool("write", output, true);
        let file_change = decoded
            .file_change
            .ok_or_else(|| std::io::Error::other("file change metadata"))?;
        match file_change.change {
            FileChange::Omitted {
                operation, reason, ..
            } => {
                assert_eq!(operation, FileChangeOperation::Update);
                assert!(reason.contains("previous file content unavailable"));
            }
            _ => panic!("expected omitted file change"),
        }
        Ok(())
    }

    #[tokio::test]
    async fn resolves_relative_path_against_cwd() -> TestResult {
        let (tool, tmp) = fixture()?;
        fs::create_dir(tmp.path().join("sub"))?;

        tool.call(args("sub/file.txt", "hello")).await?;

        assert_eq!(
            fs::read_to_string(tmp.path().join("sub/file.txt"))?,
            "hello"
        );
        Ok(())
    }

    #[tokio::test]
    async fn accepts_absolute_path_inside_cwd() -> TestResult {
        let (tool, tmp) = fixture()?;
        let path = tmp.path().join("a.txt");

        tool.call(args(path.display().to_string(), "hi")).await?;

        assert_eq!(fs::read_to_string(path)?, "hi");
        Ok(())
    }

    #[tokio::test]
    async fn rejects_path_outside_cwd() -> TestResult {
        let (tool, _tmp) = fixture()?;
        let other = TempDir::new()?;
        let path = other.path().join("file.txt");

        let Err(err) = tool.call(args(path.display().to_string(), "nope")).await else {
            panic!("write should fail");
        };

        assert!(err.to_string().contains("escapes the workspace"));
        Ok(())
    }

    #[tokio::test]
    async fn errors_when_parent_missing() -> TestResult {
        let (tool, _tmp) = fixture()?;

        let Err(err) = tool.call(args("nope/x.txt", "hello")).await else {
            panic!("write should fail");
        };

        assert!(err.to_string().contains("failed to resolve"));
        Ok(())
    }

    #[tokio::test]
    async fn writes_empty_content() -> TestResult {
        let (tool, tmp) = fixture()?;

        let output = tool.call(args("empty.txt", "")).await?;

        let path = tmp.path().join("empty.txt");
        let resolved_path = fs::canonicalize(tmp.path())?.join("empty.txt");
        assert_eq!(fs::read_to_string(&path)?, "");
        let decoded = decode_tool_output_for_tool("write", output, true);
        assert_eq!(
            decoded.model_output,
            format!("wrote 0 bytes to {}", resolved_path.display())
        );
        let file_change = decoded
            .file_change
            .ok_or_else(|| std::io::Error::other("file change metadata"))?;
        assert_eq!(
            file_change.change,
            FileChange::Add {
                content: String::new(),
            }
        );
        Ok(())
    }
}
