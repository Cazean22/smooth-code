use std::path::PathBuf;

use rig::{completion::ToolDefinition, tool::Tool};
use schemars::{JsonSchema, schema_for};
use serde::Deserialize;

use crate::ToolError;

const DEFAULT_LIMIT: usize = 2000;
const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

const DESCRIPTION: &str = r#"Read a UTF-8 text file from the local filesystem. Assume this tool can reach any file on the machine; if a path is provided, treat it as valid. It is okay to read a file that does not exist - an error will be returned.

Usage:
- `file_path` may be an absolute path, or a path relative to the current working directory.
- By default, the tool reads up to {default_limit} lines starting at the beginning of the file. Prefer reading the whole file by omitting `offset` and `limit`; use them only when the file is too large to read in one go.
- `offset` is a zero-based count of lines to skip: omit it or set `offset: 0` to start at the first line; `offset: 1` starts at line 2.
- Results are returned in `cat -n` format, with line numbers starting at 1.
- This tool only reads files, not directories. For directories, use shell commands such as `eza` when `run_command` is available.
- If the file exists but is empty, the tool returns a marker indicating that the file is empty."#;

#[derive(Clone)]
pub struct ReadTool {
    cwd: PathBuf,
    default_limit: usize,
}

impl ReadTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            default_limit: DEFAULT_LIMIT,
        }
    }

    /// Override the default line limit (from the resolved app config).
    pub fn with_default_limit(mut self, default_limit: usize) -> Self {
        self.default_limit = default_limit;
        self
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadArgs {
    /// Absolute path to the file, or a path relative to the current working directory.
    file_path: String,

    /// Zero-based line offset, i.e. the number of lines to skip before reading.
    /// Omit or use 0 to start at the first line; offset 1 starts at line 2.
    /// Only provide if the file is too large to read at once.
    #[serde(default)]
    #[schemars(range(min = 0, max = MAX_SAFE_INTEGER))]
    offset: Option<usize>,

    /// The number of lines to read. Only provide if the file is too large to read at once.
    #[serde(default)]
    #[schemars(range(min = 1, max = MAX_SAFE_INTEGER))]
    limit: Option<usize>,
}

impl Tool for ReadTool {
    const NAME: &'static str = "read";

    type Error = ToolError;
    type Args = ReadArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: DESCRIPTION.replace("{default_limit}", &self.default_limit.to_string()),
            parameters: schema_for!(ReadArgs).to_value(),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let path = self.cwd.join(&args.file_path);

        let content = std::fs::read_to_string(&path)
            .map_err(|err| ToolError::io(format!("failed to read {}: {err}", path.display())))?;

        if content.is_empty() {
            return Ok("<file is empty>".to_string());
        }

        let skip_count = args.offset.unwrap_or(0);
        let limit = args.limit.unwrap_or(self.default_limit);
        let display_start = skip_count.saturating_add(1);

        let formatted = content
            .lines()
            .skip(skip_count)
            .take(limit)
            .enumerate()
            .map(|(i, line)| format!("{:>6}\t{}", display_start + i, line))
            .collect::<Vec<_>>()
            .join("\n");

        Ok(formatted)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn fixture() -> Result<(ReadTool, TempDir), std::io::Error> {
        let tmp = TempDir::new()?;
        let tool = ReadTool::new(tmp.path().to_path_buf());
        Ok((tool, tmp))
    }

    fn args(file_path: impl Into<String>) -> ReadArgs {
        ReadArgs {
            file_path: file_path.into(),
            offset: None,
            limit: None,
        }
    }

    #[tokio::test]
    async fn formats_content_in_cat_n_style() -> TestResult {
        let (tool, tmp) = fixture()?;
        fs::write(tmp.path().join("foo.txt"), "alpha\nbeta\ngamma")?;

        let output = tool.call(args("foo.txt")).await?;
        assert_eq!(output, "     1\talpha\n     2\tbeta\n     3\tgamma");
        Ok(())
    }

    #[tokio::test]
    async fn resolves_relative_path_against_cwd() -> TestResult {
        let (tool, tmp) = fixture()?;
        fs::create_dir(tmp.path().join("sub"))?;
        fs::write(tmp.path().join("sub/file.txt"), "hello")?;

        let output = tool.call(args("sub/file.txt")).await?;
        assert_eq!(output, "     1\thello");
        Ok(())
    }

    #[tokio::test]
    async fn accepts_absolute_path_outside_cwd() -> TestResult {
        let (tool, _tmp) = fixture()?;
        let other = TempDir::new()?;
        let abs = other.path().join("file.txt");
        fs::write(&abs, "hi")?;

        let output = tool.call(args(abs.display().to_string())).await?;
        assert_eq!(output, "     1\thi");
        Ok(())
    }

    #[tokio::test]
    async fn applies_offset_and_limit() -> TestResult {
        let (tool, tmp) = fixture()?;
        fs::write(tmp.path().join("nums.txt"), "1\n2\n3\n4\n5")?;

        let output = tool
            .call(ReadArgs {
                file_path: "nums.txt".to_string(),
                offset: Some(2),
                limit: Some(2),
            })
            .await?;
        assert_eq!(output, "     3\t3\n     4\t4");
        Ok(())
    }

    #[tokio::test]
    async fn returns_marker_for_empty_file() -> TestResult {
        let (tool, tmp) = fixture()?;
        fs::write(tmp.path().join("empty.txt"), "")?;

        let output = tool.call(args("empty.txt")).await?;
        assert_eq!(output, "<file is empty>");
        Ok(())
    }

    #[tokio::test]
    async fn errors_on_missing_file() -> TestResult {
        let (tool, _tmp) = fixture()?;

        let Err(err) = tool.call(args("missing.txt")).await else {
            panic!("read should fail");
        };
        assert!(err.to_string().contains("failed to read"));
        Ok(())
    }
}
