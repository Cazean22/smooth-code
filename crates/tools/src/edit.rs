use std::{fs, path::Path, path::PathBuf};

use rig::{completion::ToolDefinition, tool::Tool};
use schemars::{JsonSchema, schema_for};
use serde::Deserialize;
use smooth_protocol::{FileChange, FileChangeOperation, FileChangeOutput};

use crate::{
    MAX_FILE_CHANGE_BYTES, ToolFailure, encode_tool_output, shared::resolve_path_for_write,
};

const DESCRIPTION: &str = r#"Perform exact string replacements in a UTF-8 text file.

Usage:
- Read the file with the `read` tool first so `old_string` values are exact, including indentation. The line-number prefix from `read` output (line number + tab) is NOT part of the file content - never include it in `old_string` or `new_string`.
- Provide a non-empty `replacements` array. For a single edit, pass a one-item array.
- Each replacement fails if `old_string` is not unique in the current file content. Either include more surrounding context to disambiguate, or set `replace_all` to rename every occurrence.
- Replacements in the array are applied in order to in-memory content, then written once after all replacements validate.
- This tool only modifies existing files; use the `write` tool to create new files.
- `file_path` may be absolute or relative to the current working directory, but must resolve inside the workspace."#;

#[derive(Clone)]
pub struct EditTool {
    cwd: PathBuf,
}

impl EditTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self { cwd }
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EditArgs {
    /// Absolute path to the file, or a path relative to the current working directory.
    file_path: String,

    /// Ordered replacements to apply to the file in one write.
    replacements: Vec<Replacement>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct Replacement {
    /// The text to replace.
    old_string: String,

    /// The text to replace it with (must be different from old_string).
    new_string: String,

    /// Replace all occurrences of old_string (default false).
    #[serde(default)]
    replace_all: bool,
}

impl Tool for EditTool {
    const NAME: &'static str = "edit";

    type Error = ToolFailure;
    type Args = EditArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: DESCRIPTION.to_string(),
            parameters: schema_for!(EditArgs).to_value(),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let EditArgs {
            file_path,
            replacements,
        } = args;

        if replacements.is_empty() {
            return Err(ToolFailure::new("replacements must not be empty"));
        }

        let path = resolve_path_for_write(&self.cwd, &file_path)?;

        let content = fs::read_to_string(&path)
            .map_err(|err| ToolFailure::new(format!("failed to read {}: {err}", path.display())))?;

        let mut new_content = content.clone();
        let mut replacement_count = 0;
        for (index, replacement) in replacements.iter().enumerate() {
            let count = validate_replacement(&new_content, replacement, index, &path)?;
            new_content = apply_replacement(new_content, replacement);
            replacement_count += count;
        }

        fs::write(&path, &new_content).map_err(|err| {
            ToolFailure::new(format!("failed to write {}: {err}", path.display()))
        })?;

        let plural_suffix = if replacement_count == 1 { "" } else { "s" };
        let model_output = format!(
            "edited {} ({} replacement{})",
            path.display(),
            replacement_count,
            plural_suffix
        );
        let unified_diff = diffy::create_patch(&content, &new_content).to_string();
        let (added, removed) = diff_line_counts(&unified_diff);
        let change = if unified_diff.len() > MAX_FILE_CHANGE_BYTES {
            FileChange::Omitted {
                operation: FileChangeOperation::Update,
                reason: format!(
                    "diff omitted because it exceeds {} bytes",
                    MAX_FILE_CHANGE_BYTES
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
        };
        let file_change = FileChangeOutput { path, change };
        Ok(encode_tool_output(model_output, Some(file_change)))
    }
}

fn validate_replacement(
    content: &str,
    replacement: &Replacement,
    index: usize,
    path: &Path,
) -> Result<usize, ToolFailure> {
    let old_label = replacement_field_label(index, "old_string");
    let new_label = replacement_field_label(index, "new_string");

    if replacement.old_string.is_empty() {
        return Err(ToolFailure::new(format!("{old_label} must not be empty")));
    }

    if replacement.old_string == replacement.new_string {
        return Err(ToolFailure::new(format!(
            "{old_label} and {new_label} must differ"
        )));
    }

    let replacement_count = content.matches(&replacement.old_string).count();
    if replacement_count == 0 {
        return Err(ToolFailure::new(format!(
            "{old_label} not found in {}",
            path.display()
        )));
    }

    if replacement_count > 1 && !replacement.replace_all {
        return Err(ToolFailure::new(format!(
            "{old_label} is not unique in {} ({} matches); set replace_all=true on that replacement or include more surrounding context",
            path.display(),
            replacement_count
        )));
    }

    Ok(replacement_count)
}

fn replacement_field_label(index: usize, field: &str) -> String {
    format!("replacement[{index}].{field}")
}

fn apply_replacement(content: String, replacement: &Replacement) -> String {
    if replacement.replace_all {
        content.replace(&replacement.old_string, &replacement.new_string)
    } else {
        content.replacen(&replacement.old_string, &replacement.new_string, 1)
    }
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

    fn fixture() -> (EditTool, TempDir) {
        let tmp = TempDir::new().expect("create tempdir");
        let tool = EditTool::new(tmp.path().to_path_buf());
        (tool, tmp)
    }

    fn args(
        file_path: impl Into<String>,
        old_string: impl Into<String>,
        new_string: impl Into<String>,
    ) -> EditArgs {
        EditArgs {
            file_path: file_path.into(),
            replacements: vec![replacement(old_string, new_string)],
        }
    }

    fn args_with_replacements(
        file_path: impl Into<String>,
        replacements: Vec<Replacement>,
    ) -> EditArgs {
        EditArgs {
            file_path: file_path.into(),
            replacements,
        }
    }

    fn replacement(old_string: impl Into<String>, new_string: impl Into<String>) -> Replacement {
        Replacement {
            old_string: old_string.into(),
            new_string: new_string.into(),
            replace_all: false,
        }
    }

    #[test]
    fn deserializes_replacements_array_args() {
        let args: EditArgs = serde_json::from_value(serde_json::json!({
            "file_path": "foo.txt",
            "replacements": [
                {
                    "old_string": "world",
                    "new_string": "there",
                    "replace_all": true
                }
            ]
        }))
        .expect("deserialize edit args");

        assert_eq!(args.file_path, "foo.txt");
        assert_eq!(args.replacements.len(), 1);
        assert_eq!(args.replacements[0].old_string, "world");
        assert_eq!(args.replacements[0].new_string, "there");
        assert!(args.replacements[0].replace_all);
    }

    #[test]
    fn rejects_unknown_args() {
        let err = match serde_json::from_value::<EditArgs>(serde_json::json!({
            "file_path": "foo.txt",
            "replacements": [
                {
                    "old_string": "world",
                    "new_string": "there"
                }
            ],
            "unexpected": true
        })) {
            Ok(_) => panic!("unknown fields should fail"),
            Err(err) => err,
        };

        assert!(err.to_string().contains("unknown field"));
    }

    #[test]
    fn rejects_missing_replacements_arg() {
        let err = match serde_json::from_value::<EditArgs>(serde_json::json!({
            "file_path": "foo.txt"
        })) {
            Ok(_) => panic!("missing replacements should fail"),
            Err(err) => err,
        };

        assert!(err.to_string().contains("missing field `replacements`"));
    }

    #[tokio::test]
    async fn replaces_unique_match() {
        let (tool, tmp) = fixture();
        let path = tmp.path().join("foo.txt");
        fs::write(&path, "hello world\n").unwrap();

        let output = tool
            .call(args("foo.txt", "world", "there"))
            .await
            .expect("edit should succeed");

        let resolved_path = fs::canonicalize(tmp.path()).unwrap().join("foo.txt");
        assert_eq!(fs::read_to_string(path).unwrap(), "hello there\n");
        let decoded = decode_tool_output_for_tool("edit", output, true);
        assert_eq!(
            decoded.model_output,
            format!("edited {} (1 replacement)", resolved_path.display())
        );
        let file_change = decoded.file_change.expect("file change metadata");
        assert_eq!(file_change.path, resolved_path);
        match file_change.change {
            FileChange::Update { unified_diff, .. } => {
                assert!(unified_diff.contains("-hello world"));
                assert!(unified_diff.contains("+hello there"));
            }
            _ => panic!("expected update file change"),
        }
    }

    #[tokio::test]
    async fn replaces_all_when_replace_all_true() {
        let (tool, tmp) = fixture();
        let path = tmp.path().join("foo.txt");
        fs::write(&path, "hello world\nworld\n").unwrap();

        let output = tool
            .call(EditArgs {
                file_path: "foo.txt".to_string(),
                replacements: vec![Replacement {
                    old_string: "world".to_string(),
                    new_string: "there".to_string(),
                    replace_all: true,
                }],
            })
            .await
            .expect("edit should succeed");

        let resolved_path = fs::canonicalize(tmp.path()).unwrap().join("foo.txt");
        assert_eq!(fs::read_to_string(path).unwrap(), "hello there\nthere\n");
        let decoded = decode_tool_output_for_tool("edit", output, true);
        assert_eq!(
            decoded.model_output,
            format!("edited {} (2 replacements)", resolved_path.display())
        );
        let file_change = decoded.file_change.expect("file change metadata");
        match file_change.change {
            FileChange::Update { unified_diff, .. } => {
                assert!(unified_diff.contains("-hello world"));
                assert!(unified_diff.contains("+hello there"));
                assert!(unified_diff.contains("+there"));
            }
            _ => panic!("expected update file change"),
        }
    }

    #[tokio::test]
    async fn applies_ordered_multi_replacements_with_single_diff() {
        let (tool, tmp) = fixture();
        let path = tmp.path().join("foo.txt");
        fs::write(&path, "red blue green\n").unwrap();

        let output = tool
            .call(args_with_replacements(
                "foo.txt",
                vec![
                    replacement("red blue", "blue red"),
                    replacement("blue red green", "done"),
                ],
            ))
            .await
            .expect("edit should succeed");

        let resolved_path = fs::canonicalize(tmp.path()).unwrap().join("foo.txt");
        assert_eq!(fs::read_to_string(path).unwrap(), "done\n");
        let decoded = decode_tool_output_for_tool("edit", output, true);
        assert_eq!(
            decoded.model_output,
            format!("edited {} (2 replacements)", resolved_path.display())
        );
        match decoded.file_change.expect("file change metadata").change {
            FileChange::Update { unified_diff, .. } => {
                assert!(unified_diff.contains("-red blue green"));
                assert!(unified_diff.contains("+done"));
                assert!(!unified_diff.contains("+blue red green"));
            }
            _ => panic!("expected update file change"),
        }
    }

    #[tokio::test]
    async fn rolls_back_multi_replacement_when_later_replacement_is_not_unique() {
        let (tool, tmp) = fixture();
        let path = tmp.path().join("foo.txt");
        fs::write(&path, "alpha beta beta\n").unwrap();

        let err = tool
            .call(args_with_replacements(
                "foo.txt",
                vec![replacement("alpha", "done"), replacement("beta", "gamma")],
            ))
            .await
            .expect_err("edit should fail");

        assert!(err.to_string().contains("replacement[1].old_string"));
        assert!(err.to_string().contains("is not unique"));
        assert_eq!(fs::read_to_string(path).unwrap(), "alpha beta beta\n");
    }

    #[tokio::test]
    async fn rolls_back_multi_replacement_when_later_replacement_is_missing() {
        let (tool, tmp) = fixture();
        let path = tmp.path().join("foo.txt");
        fs::write(&path, "alpha beta\n").unwrap();

        let err = tool
            .call(args_with_replacements(
                "foo.txt",
                vec![
                    replacement("alpha", "done"),
                    replacement("missing", "gamma"),
                ],
            ))
            .await
            .expect_err("edit should fail");

        assert!(err.to_string().contains("replacement[1].old_string"));
        assert!(err.to_string().contains("not found"));
        assert_eq!(fs::read_to_string(path).unwrap(), "alpha beta\n");
    }

    #[tokio::test]
    async fn errors_when_replacements_array_is_empty() {
        let (tool, tmp) = fixture();
        fs::write(tmp.path().join("foo.txt"), "hello\n").unwrap();

        let err = tool
            .call(args_with_replacements("foo.txt", Vec::new()))
            .await
            .expect_err("edit should fail");

        assert_eq!(err.to_string(), "replacements must not be empty");
    }

    #[tokio::test]
    async fn errors_when_old_string_not_unique() {
        let (tool, tmp) = fixture();
        fs::write(tmp.path().join("foo.txt"), "world\nworld\n").unwrap();

        let err = tool
            .call(args("foo.txt", "world", "there"))
            .await
            .expect_err("edit should fail");

        assert!(err.to_string().contains("old_string is not unique"));
        assert!(err.to_string().contains("set replace_all=true"));
    }

    #[tokio::test]
    async fn errors_when_old_string_missing() {
        let (tool, tmp) = fixture();
        fs::write(tmp.path().join("foo.txt"), "hello\n").unwrap();

        let err = tool
            .call(args("foo.txt", "world", "there"))
            .await
            .expect_err("edit should fail");

        assert!(err.to_string().contains("old_string not found"));
    }

    #[tokio::test]
    async fn errors_when_old_equals_new() {
        let (tool, tmp) = fixture();
        fs::write(tmp.path().join("foo.txt"), "hello\n").unwrap();

        let err = tool
            .call(args("foo.txt", "hello", "hello"))
            .await
            .expect_err("edit should fail");

        assert_eq!(
            err.to_string(),
            "replacement[0].old_string and replacement[0].new_string must differ"
        );
    }

    #[tokio::test]
    async fn errors_when_old_string_empty() {
        let (tool, tmp) = fixture();
        fs::write(tmp.path().join("foo.txt"), "hello\n").unwrap();

        let err = tool
            .call(args("foo.txt", "", "hello"))
            .await
            .expect_err("edit should fail");

        assert_eq!(
            err.to_string(),
            "replacement[0].old_string must not be empty"
        );
    }

    #[tokio::test]
    async fn errors_when_file_missing() {
        let (tool, _tmp) = fixture();

        let err = tool
            .call(args("missing.txt", "hello", "hi"))
            .await
            .expect_err("edit should fail");

        assert!(err.to_string().contains("failed to read"));
    }

    #[tokio::test]
    async fn rejects_path_outside_workspace() {
        let (tool, _tmp) = fixture();
        let other = TempDir::new().expect("create second tempdir");
        let path = other.path().join("file.txt");
        fs::write(&path, "hello\n").unwrap();

        let err = tool
            .call(args(path.display().to_string(), "hello", "hi"))
            .await
            .expect_err("edit should fail");

        assert!(err.to_string().contains("escapes the workspace"));
    }

    #[tokio::test]
    async fn resolves_relative_path_against_cwd() {
        let (tool, tmp) = fixture();
        fs::create_dir(tmp.path().join("sub")).unwrap();
        let path = tmp.path().join("sub/file.txt");
        fs::write(&path, "alpha beta\n").unwrap();

        tool.call(args("sub/file.txt", "beta", "gamma"))
            .await
            .expect("edit should succeed");

        assert_eq!(fs::read_to_string(path).unwrap(), "alpha gamma\n");
    }

    #[tokio::test]
    async fn accepts_absolute_path_inside_cwd() {
        let (tool, tmp) = fixture();
        let path = tmp.path().join("foo.txt");
        fs::write(&path, "hello\n").unwrap();

        tool.call(args(path.display().to_string(), "hello", "hi"))
            .await
            .expect("edit should succeed");

        assert_eq!(fs::read_to_string(path).unwrap(), "hi\n");
    }
}
