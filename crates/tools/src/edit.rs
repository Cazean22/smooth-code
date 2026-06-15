use std::{fs, io::ErrorKind, path::PathBuf};

use cazean_protocol::{FileChange, FileChangeOperation, FileChangeOutput};
use rig::{completion::ToolDefinition, tool::Tool};
use schemars::{JsonSchema, schema_for};
use serde::Deserialize;

use crate::{
    MAX_FILE_CHANGE_BYTES, ToolError, encode_tool_output_with_file_changes,
    shared::resolve_path_for_write,
};

const DESCRIPTION: &str = r#"Edit one existing UTF-8 text file with sed-style literal replacements.

Call shape:
{"file_path":"path/to/file.rs","replacements":[{"old_string":"exact text to find","new_string":"replacement text","replace_all":false}],"move_to":"optional/new/path.rs"}

Usage:
- `file_path` may be absolute or relative to the current working directory, but must resolve inside the workspace.
- The target must be an existing UTF-8 file. Use `write` to create files and `delete` to remove files.
- `replacements` are applied sequentially to the in-memory file content before any filesystem mutation.
- `old_string` is a literal string, not a regex or shell/sed script. It may span multiple lines and must not be empty.
- `new_string` is inserted exactly as provided and may be empty to delete text.
- `replace_all` defaults to false. When false, `old_string` must match exactly once; when true, every match is replaced and at least one match is required.
- Include enough surrounding text in `old_string` to make a targeted replacement unique, or set `replace_all` to true for a deliberate global replacement.
- Optional `move_to` renames the file after replacements. The target must not already exist unless it is the same path as `file_path`.
- Empty `replacements` are allowed only when `move_to` performs a real move.
- Edits that do not change content or move the file fail. Preparation happens before filesystem mutation; apply-phase filesystem failures are not transactionally rolled back."#;

#[derive(Clone)]
pub struct EditTool {
    cwd: PathBuf,
    max_file_change_bytes: usize,
}

impl EditTool {
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

#[derive(Debug, Clone, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EditArgs {
    /// Absolute path to an existing UTF-8 file, or a path relative to the current working directory.
    file_path: String,

    /// Ordered sed-style literal substitutions to apply before any optional move.
    #[serde(default)]
    replacements: Vec<ReplacementArgs>,

    /// Optional destination path for a rename/move. The target must not already exist unless it is
    /// the same path as file_path.
    move_to: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ReplacementArgs {
    /// Literal text to find. This is not a regex and may span multiple lines.
    old_string: String,

    /// Replacement text. May be empty to delete the matched text.
    new_string: String,

    /// Replace every match. If false or omitted, old_string must match exactly once.
    #[serde(default)]
    replace_all: bool,
}

impl Tool for EditTool {
    const NAME: &'static str = "edit";

    type Error = ToolError;
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
        let prepared = prepare_edit(&self.cwd, args)?;
        let file_changes = vec![prepared.file_change(self.max_file_change_bytes)];
        prepared.apply()?;

        let model_output = String::from("applied edits (1 file changed)");
        Ok(encode_tool_output_with_file_changes(
            model_output,
            file_changes,
        ))
    }
}

#[derive(Debug, Clone)]
struct PreparedChange {
    path: PathBuf,
    move_to: Option<PathBuf>,
    original_content: String,
    new_content: String,
}

impl PreparedChange {
    fn apply(self) -> Result<(), ToolError> {
        if self.original_content != self.new_content {
            fs::write(&self.path, &self.new_content).map_err(|err| {
                ToolError::io(format!("failed to write {}: {err}", self.path.display()))
            })?;
        }
        if let Some(move_to) = self.move_to
            && move_to != self.path
        {
            fs::rename(&self.path, &move_to).map_err(|err| {
                ToolError::io(format!(
                    "failed to move {} to {}: {err}",
                    self.path.display(),
                    move_to.display()
                ))
            })?;
        }
        Ok(())
    }

    fn file_change(&self, max_file_change_bytes: usize) -> FileChangeOutput {
        FileChangeOutput {
            path: self.path.clone(),
            change: update_file_change(
                &self.original_content,
                &self.new_content,
                self.move_to.clone(),
                max_file_change_bytes,
            ),
        }
    }
}

fn prepare_edit(cwd: &std::path::Path, args: EditArgs) -> Result<PreparedChange, ToolError> {
    validate_path_arg(&args.file_path, "file_path")?;
    if let Some(move_to) = args.move_to.as_deref() {
        validate_path_arg(move_to, "move_to")?;
    }

    let path = resolve_path_for_write(cwd, &args.file_path)?;
    validate_existing_file(&path, "edit")?;
    let original_content = read_utf8_file(&path, "edit")?;
    let mut new_content = original_content.clone();

    for (index, replacement) in args.replacements.iter().enumerate() {
        apply_replacement(&path, &mut new_content, replacement, index)?;
    }

    let move_to = args
        .move_to
        .as_deref()
        .map(|move_to| resolve_move_target(cwd, &path, move_to))
        .transpose()?
        .filter(|move_to| move_to != &path);

    if args.replacements.is_empty() && move_to.is_none() {
        return Err(ToolError::invalid_arguments(
            "edit must include at least one replacement or move_to",
        ));
    }

    if move_to.is_none() && original_content == new_content {
        return Err(ToolError::invalid_arguments(format!(
            "edit file {} produced no changes",
            path.display()
        )));
    }

    Ok(PreparedChange {
        path,
        move_to,
        original_content,
        new_content,
    })
}

fn validate_path_arg(path: &str, label: &str) -> Result<(), ToolError> {
    if path.is_empty() {
        return Err(ToolError::invalid_arguments(format!(
            "{label} must not be empty"
        )));
    }
    if path.trim() != path {
        return Err(ToolError::invalid_arguments(format!(
            "{label} must not have leading or trailing whitespace: {path:?}"
        )));
    }
    Ok(())
}

fn validate_existing_file(path: &std::path::Path, operation: &str) -> Result<(), ToolError> {
    let metadata = fs::metadata(path).map_err(|err| {
        if err.kind() == ErrorKind::NotFound {
            ToolError::io(format!("file {} does not exist", path.display()))
        } else {
            ToolError::io(format!("failed to inspect {}: {err}", path.display()))
        }
    })?;
    if metadata.is_dir() {
        return Err(ToolError::invalid_arguments(format!(
            "{} is a directory; cannot {operation}",
            path.display()
        )));
    }
    Ok(())
}

fn read_utf8_file(path: &std::path::Path, operation: &str) -> Result<String, ToolError> {
    fs::read_to_string(path).map_err(|err| {
        if err.kind() == ErrorKind::InvalidData {
            ToolError::invalid_arguments(format!(
                "{operation} input {} is not valid UTF-8",
                path.display()
            ))
        } else {
            ToolError::io(format!("failed to read {}: {err}", path.display()))
        }
    })
}

fn resolve_move_target(
    cwd: &std::path::Path,
    source: &std::path::Path,
    move_to: &str,
) -> Result<PathBuf, ToolError> {
    let target = resolve_path_for_write(cwd, move_to)?;
    if target != source {
        match fs::metadata(&target) {
            Ok(_) => {
                return Err(ToolError::invalid_arguments(format!(
                    "move target {} already exists",
                    target.display()
                )));
            }
            Err(err) if err.kind() == ErrorKind::NotFound => {}
            Err(err) => {
                return Err(ToolError::io(format!(
                    "failed to inspect {}: {err}",
                    target.display()
                )));
            }
        }
    }
    Ok(target)
}

fn apply_replacement(
    path: &std::path::Path,
    content: &mut String,
    replacement: &ReplacementArgs,
    index: usize,
) -> Result<(), ToolError> {
    if replacement.old_string.is_empty() {
        return Err(ToolError::invalid_arguments(format!(
            "replacements[{index}].old_string must not be empty"
        )));
    }
    if replacement.old_string == replacement.new_string {
        return Err(ToolError::invalid_arguments(format!(
            "replacements[{index}] old_string and new_string must differ"
        )));
    }

    let matches = content.matches(&replacement.old_string).count();
    if matches == 0 {
        return Err(ToolError::invalid_arguments(format!(
            "replacement {index} old_string not found in {}",
            path.display()
        )));
    }
    if !replacement.replace_all && matches > 1 {
        return Err(ToolError::invalid_arguments(format!(
            "replacement {index} old_string is ambiguous in {} ({matches} matches); provide more context or set replace_all to true",
            path.display()
        )));
    }

    let replaced = if replacement.replace_all {
        content.replace(&replacement.old_string, &replacement.new_string)
    } else {
        content.replacen(&replacement.old_string, &replacement.new_string, 1)
    };
    *content = replaced;
    Ok(())
}

fn update_file_change(
    original_content: &str,
    new_content: &str,
    move_path: Option<PathBuf>,
    max_file_change_bytes: usize,
) -> FileChange {
    let unified_diff = if original_content == new_content {
        String::new()
    } else {
        diffy::create_patch(original_content, new_content).to_string()
    };
    let (added, removed) = diff_line_counts(&unified_diff);
    if unified_diff.len() > max_file_change_bytes {
        FileChange::Omitted {
            operation: FileChangeOperation::Update,
            reason: format!(
                "diff omitted because it exceeds {} bytes",
                max_file_change_bytes
            ),
            added,
            removed,
            bytes: unified_diff.len(),
        }
    } else {
        FileChange::Update {
            unified_diff,
            move_path,
        }
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
    use serde_json::Value;
    use tempfile::TempDir;

    use crate::decode_tool_output_for_tool;

    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn fixture() -> Result<(EditTool, TempDir), std::io::Error> {
        let tmp = TempDir::new()?;
        let tool = EditTool::new(tmp.path().to_path_buf());
        Ok((tool, tmp))
    }

    fn args(
        file_path: impl Into<String>,
        replacements: Vec<ReplacementArgs>,
        move_to: Option<&str>,
    ) -> EditArgs {
        EditArgs {
            file_path: file_path.into(),
            replacements,
            move_to: move_to.map(str::to_string),
        }
    }

    fn replacement(
        old_string: impl Into<String>,
        new_string: impl Into<String>,
    ) -> ReplacementArgs {
        ReplacementArgs {
            old_string: old_string.into(),
            new_string: new_string.into(),
            replace_all: false,
        }
    }

    fn replacement_all(
        old_string: impl Into<String>,
        new_string: impl Into<String>,
    ) -> ReplacementArgs {
        ReplacementArgs {
            old_string: old_string.into(),
            new_string: new_string.into(),
            replace_all: true,
        }
    }

    fn replacement_args(
        file_path: impl Into<String>,
        old_string: impl Into<String>,
        new_string: impl Into<String>,
    ) -> EditArgs {
        args(file_path, vec![replacement(old_string, new_string)], None)
    }

    fn decoded_changes(
        output: String,
    ) -> Result<crate::DecodedToolOutput, Box<dyn std::error::Error>> {
        let decoded = decode_tool_output_for_tool("edit", output, true);
        if decoded.file_changes.is_empty() {
            return Err(std::io::Error::other("file change metadata").into());
        }
        Ok(decoded)
    }

    fn deserialize_error(value: Value) -> String {
        match serde_json::from_value::<EditArgs>(value) {
            Ok(_) => panic!("edit args should fail"),
            Err(err) => err.to_string(),
        }
    }

    #[test]
    fn deserializes_sed_style_args() -> TestResult {
        let args: EditArgs = serde_json::from_value(serde_json::json!({
            "file_path": "foo.txt",
            "move_to": "bar.txt",
            "replacements": [
                {
                    "old_string": "world",
                    "new_string": "there",
                    "replace_all": true
                }
            ]
        }))?;

        assert_eq!(args.file_path, "foo.txt");
        assert_eq!(args.move_to.as_deref(), Some("bar.txt"));
        assert_eq!(args.replacements.len(), 1);
        assert_eq!(args.replacements[0].old_string, "world");
        assert_eq!(args.replacements[0].new_string, "there");
        assert!(args.replacements[0].replace_all);
        Ok(())
    }

    #[test]
    fn defaults_replace_all_to_false() -> TestResult {
        let args: EditArgs = serde_json::from_value(serde_json::json!({
            "file_path": "foo.txt",
            "replacements": [
                {
                    "old_string": "world",
                    "new_string": "there"
                }
            ]
        }))?;

        assert!(!args.replacements[0].replace_all);
        Ok(())
    }

    #[test]
    fn rejects_old_patch_args() {
        let message = deserialize_error(serde_json::json!({
            "patch": "*** Begin Patch\n*** Update File: foo.txt\n@@\n-old\n+new\n*** End Patch\n"
        }));

        assert!(
            message.contains("missing field `file_path`") || message.contains("unknown field"),
            "{message}"
        );
    }

    #[test]
    fn rejects_old_hunk_args() {
        let message = deserialize_error(serde_json::json!({
            "updates": [
                {
                    "file_path": "foo.txt",
                    "hunks": [
                        {
                            "lines": [
                                { "kind": "remove", "text": "old" },
                                { "kind": "add", "text": "new" }
                            ]
                        }
                    ]
                }
            ]
        }));

        assert!(
            message.contains("missing field `file_path`") || message.contains("unknown field"),
            "{message}"
        );
    }

    #[test]
    fn rejects_unknown_args() {
        for value in [
            serde_json::json!({
                "file_path": "foo.txt",
                "replacements": [],
                "unexpected": true
            }),
            serde_json::json!({
                "file_path": "foo.txt",
                "replacements": [
                    {
                        "old_string": "old",
                        "new_string": "new",
                        "unexpected": true
                    }
                ]
            }),
        ] {
            let message = deserialize_error(value);
            assert!(message.contains("unknown field"), "{message}");
        }
    }

    #[tokio::test]
    async fn applies_single_replacement() -> TestResult {
        let (tool, tmp) = fixture()?;
        let path = tmp.path().join("foo.txt");
        fs::write(&path, "hello world\n")?;

        let output = tool
            .call(replacement_args("foo.txt", "hello world", "hello there"))
            .await?;

        let resolved_path = fs::canonicalize(tmp.path())?.join("foo.txt");
        assert_eq!(fs::read_to_string(path)?, "hello there\n");
        let decoded = decoded_changes(output)?;
        assert_eq!(decoded.model_output, "applied edits (1 file changed)");
        assert_eq!(decoded.file_changes.len(), 1);
        assert_eq!(
            decoded.file_change.as_ref().map(|change| &change.path),
            Some(&resolved_path)
        );
        match &decoded.file_changes[0].change {
            FileChange::Update { unified_diff, .. } => {
                assert!(unified_diff.contains("-hello world"));
                assert!(unified_diff.contains("+hello there"));
            }
            _ => panic!("expected update file change"),
        }
        Ok(())
    }

    #[tokio::test]
    async fn applies_multiple_sequential_replacements() -> TestResult {
        let (tool, tmp) = fixture()?;
        let path = tmp.path().join("foo.txt");
        fs::write(&path, "one two three\n")?;

        tool.call(args(
            "foo.txt",
            vec![replacement("one", "uno"), replacement("three", "tres")],
            None,
        ))
        .await?;

        assert_eq!(fs::read_to_string(path)?, "uno two tres\n");
        Ok(())
    }

    #[tokio::test]
    async fn replacements_are_applied_to_current_in_memory_content() -> TestResult {
        let (tool, tmp) = fixture()?;
        let path = tmp.path().join("foo.txt");
        fs::write(&path, "alpha\n")?;

        tool.call(args(
            "foo.txt",
            vec![replacement("alpha", "beta"), replacement("beta", "gamma")],
            None,
        ))
        .await?;

        assert_eq!(fs::read_to_string(path)?, "gamma\n");
        Ok(())
    }

    #[tokio::test]
    async fn applies_multiline_replacement() -> TestResult {
        let (tool, tmp) = fixture()?;
        let path = tmp.path().join("foo.txt");
        fs::write(&path, "alpha\nbeta\ngamma\n")?;

        tool.call(replacement_args(
            "foo.txt",
            "alpha\nbeta",
            "alpha\ninserted\nbeta",
        ))
        .await?;

        assert_eq!(fs::read_to_string(path)?, "alpha\ninserted\nbeta\ngamma\n");
        Ok(())
    }

    #[tokio::test]
    async fn deletes_text_with_empty_new_string() -> TestResult {
        let (tool, tmp) = fixture()?;
        let path = tmp.path().join("foo.txt");
        fs::write(&path, "alpha beta gamma\n")?;

        tool.call(replacement_args("foo.txt", " beta", "")).await?;

        assert_eq!(fs::read_to_string(path)?, "alpha gamma\n");
        Ok(())
    }

    #[tokio::test]
    async fn replace_all_replaces_every_match() -> TestResult {
        let (tool, tmp) = fixture()?;
        let path = tmp.path().join("foo.txt");
        fs::write(&path, "same\nsame\n")?;

        tool.call(args(
            "foo.txt",
            vec![replacement_all("same", "changed")],
            None,
        ))
        .await?;

        assert_eq!(fs::read_to_string(path)?, "changed\nchanged\n");
        Ok(())
    }

    #[tokio::test]
    async fn rejects_ambiguous_replacement_by_default() -> TestResult {
        let (tool, tmp) = fixture()?;
        let path = tmp.path().join("foo.txt");
        fs::write(&path, "same\nsame\n")?;

        let Err(err) = tool
            .call(replacement_args("foo.txt", "same", "changed"))
            .await
        else {
            panic!("edit should fail");
        };

        assert!(err.to_string().contains("ambiguous"));
        assert!(err.to_string().contains("replace_all"));
        assert_eq!(fs::read_to_string(path)?, "same\nsame\n");
        Ok(())
    }

    #[tokio::test]
    async fn rejects_missing_replacement() -> TestResult {
        let (tool, tmp) = fixture()?;
        let path = tmp.path().join("foo.txt");
        fs::write(&path, "actual\n")?;

        let Err(err) = tool.call(replacement_args("foo.txt", "old", "new")).await else {
            panic!("edit should fail");
        };

        assert!(err.to_string().contains("not found"));
        assert_eq!(fs::read_to_string(path)?, "actual\n");
        Ok(())
    }

    #[tokio::test]
    async fn rejects_empty_old_string() -> TestResult {
        let (tool, tmp) = fixture()?;
        fs::write(tmp.path().join("foo.txt"), "actual\n")?;

        let Err(err) = tool.call(replacement_args("foo.txt", "", "new")).await else {
            panic!("edit should fail");
        };

        assert!(err.to_string().contains("old_string must not be empty"));
        Ok(())
    }

    #[tokio::test]
    async fn rejects_no_op_replacement() -> TestResult {
        let (tool, tmp) = fixture()?;
        fs::write(tmp.path().join("foo.txt"), "actual\n")?;

        let Err(err) = tool
            .call(replacement_args("foo.txt", "actual", "actual"))
            .await
        else {
            panic!("edit should fail");
        };

        assert!(err.to_string().contains("must differ"));
        Ok(())
    }

    #[tokio::test]
    async fn rejects_no_effect_after_sequential_replacements() -> TestResult {
        let (tool, tmp) = fixture()?;
        fs::write(tmp.path().join("foo.txt"), "alpha\n")?;

        let Err(err) = tool
            .call(args(
                "foo.txt",
                vec![replacement("alpha", "beta"), replacement("beta", "alpha")],
                None,
            ))
            .await
        else {
            panic!("edit should fail");
        };

        assert!(err.to_string().contains("produced no changes"));
        Ok(())
    }

    #[tokio::test]
    async fn preserves_existing_line_endings_outside_replacement() -> TestResult {
        let (tool, tmp) = fixture()?;
        let path = tmp.path().join("mixed.txt");
        fs::write(&path, "alpha\r\nbeta\ngamma\r\n")?;

        tool.call(replacement_args("mixed.txt", "beta", "bee"))
            .await?;

        assert_eq!(fs::read_to_string(path)?, "alpha\r\nbee\ngamma\r\n");
        Ok(())
    }

    #[tokio::test]
    async fn preserves_missing_trailing_newline_when_replacing_last_line() -> TestResult {
        let (tool, tmp) = fixture()?;
        let path = tmp.path().join("noeol.txt");
        fs::write(&path, "alpha\nbeta")?;

        tool.call(replacement_args("noeol.txt", "beta", "BETA"))
            .await?;

        assert_eq!(fs::read_to_string(path)?, "alpha\nBETA");
        Ok(())
    }

    #[tokio::test]
    async fn applies_move_update() -> TestResult {
        let (tool, tmp) = fixture()?;
        let source = tmp.path().join("old.txt");
        let target = tmp.path().join("new.txt");
        fs::write(&source, "before\n")?;

        let output = tool
            .call(args(
                "old.txt",
                vec![replacement("before", "after")],
                Some("new.txt"),
            ))
            .await?;

        assert!(!source.exists());
        assert_eq!(fs::read_to_string(&target)?, "after\n");
        let decoded = decoded_changes(output)?;
        match &decoded.file_changes[0].change {
            FileChange::Update {
                unified_diff,
                move_path,
            } => {
                assert!(unified_diff.contains("-before"));
                assert!(unified_diff.contains("+after"));
                assert_eq!(
                    move_path.as_ref(),
                    Some(&fs::canonicalize(tmp.path())?.join("new.txt"))
                );
            }
            _ => panic!("expected update file change"),
        }
        Ok(())
    }

    #[tokio::test]
    async fn applies_rename_only_update_without_replacements() -> TestResult {
        let (tool, tmp) = fixture()?;
        let source = tmp.path().join("old.txt");
        let target = tmp.path().join("new.txt");
        fs::write(&source, "same\n")?;

        let output = tool
            .call(args("old.txt", Vec::new(), Some("new.txt")))
            .await?;

        assert!(!source.exists());
        assert_eq!(fs::read_to_string(&target)?, "same\n");
        let decoded = decoded_changes(output)?;
        match &decoded.file_changes[0].change {
            FileChange::Update {
                unified_diff,
                move_path,
            } => {
                assert!(unified_diff.is_empty());
                assert_eq!(
                    move_path.as_ref(),
                    Some(&fs::canonicalize(tmp.path())?.join("new.txt"))
                );
            }
            _ => panic!("expected update file change"),
        }
        Ok(())
    }

    #[tokio::test]
    async fn accepts_move_to_equal_file_path_with_content_change() -> TestResult {
        let (tool, tmp) = fixture()?;
        let path = tmp.path().join("same.txt");
        fs::write(&path, "before\n")?;

        let output = tool
            .call(args(
                "same.txt",
                vec![replacement("before", "after")],
                Some("same.txt"),
            ))
            .await?;

        assert_eq!(fs::read_to_string(&path)?, "after\n");
        let decoded = decoded_changes(output)?;
        match &decoded.file_changes[0].change {
            FileChange::Update { move_path, .. } => {
                assert_eq!(move_path.as_ref(), None);
            }
            _ => panic!("expected update file change"),
        }
        Ok(())
    }

    #[tokio::test]
    async fn rejects_self_move_without_content_change() -> TestResult {
        let (tool, tmp) = fixture()?;
        let path = tmp.path().join("same.txt");
        fs::write(&path, "before\n")?;

        let Err(err) = tool
            .call(args("same.txt", Vec::new(), Some("same.txt")))
            .await
        else {
            panic!("edit should fail");
        };

        assert!(err.to_string().contains("replacement or move_to"));
        assert_eq!(fs::read_to_string(path)?, "before\n");
        Ok(())
    }

    #[tokio::test]
    async fn rejects_missing_file() -> TestResult {
        let (tool, _tmp) = fixture()?;

        let Err(err) = tool
            .call(replacement_args("missing.txt", "old", "new"))
            .await
        else {
            panic!("edit should fail");
        };

        assert!(err.to_string().contains("does not exist"));
        Ok(())
    }

    #[tokio::test]
    async fn rejects_path_outside_workspace() -> TestResult {
        let (tool, _tmp) = fixture()?;
        let outside = TempDir::new()?;
        let outside_path = outside.path().join("outside.txt");
        fs::write(&outside_path, "hello\n")?;

        let Err(err) = tool
            .call(replacement_args(
                outside_path.display().to_string(),
                "hello",
                "bye",
            ))
            .await
        else {
            panic!("edit should fail");
        };

        assert!(err.to_string().contains("escapes the workspace"));
        assert_eq!(fs::read_to_string(outside_path)?, "hello\n");
        Ok(())
    }

    #[tokio::test]
    async fn rejects_non_utf8_input() -> TestResult {
        let (tool, tmp) = fixture()?;
        let path = tmp.path().join("binary.dat");
        fs::write(&path, [0xff, 0xfe])?;

        let Err(err) = tool
            .call(replacement_args("binary.dat", "old", "new"))
            .await
        else {
            panic!("edit should fail");
        };

        assert!(err.to_string().contains("not valid UTF-8"));
        assert_eq!(fs::read(path)?, [0xff, 0xfe]);
        Ok(())
    }

    #[tokio::test]
    async fn rejects_empty_replacements_without_move() -> TestResult {
        let (tool, tmp) = fixture()?;
        fs::write(tmp.path().join("foo.txt"), "hello\n")?;

        let Err(err) = tool.call(args("foo.txt", Vec::new(), None)).await else {
            panic!("edit should fail");
        };

        assert!(err.to_string().contains("replacement or move_to"));
        Ok(())
    }

    #[tokio::test]
    async fn rejects_directory_path() -> TestResult {
        let (tool, tmp) = fixture()?;
        fs::create_dir(tmp.path().join("dir"))?;

        let Err(err) = tool.call(replacement_args("dir", "old", "new")).await else {
            panic!("edit should fail");
        };

        assert!(err.to_string().contains("is a directory"));
        Ok(())
    }

    #[tokio::test]
    async fn rejects_move_target_that_already_exists() -> TestResult {
        let (tool, tmp) = fixture()?;
        let source = tmp.path().join("source.txt");
        let target = tmp.path().join("target.txt");
        fs::write(&source, "source\n")?;
        fs::write(&target, "target\n")?;

        let Err(err) = tool
            .call(args("source.txt", Vec::new(), Some("target.txt")))
            .await
        else {
            panic!("edit should fail");
        };

        assert!(err.to_string().contains("move target"));
        assert_eq!(fs::read_to_string(source)?, "source\n");
        assert_eq!(fs::read_to_string(target)?, "target\n");
        Ok(())
    }

    #[tokio::test]
    async fn prepares_every_replacement_before_writing() -> TestResult {
        let (tool, tmp) = fixture()?;
        let path = tmp.path().join("foo.txt");
        fs::write(&path, "old\nactual\n")?;

        let Err(err) = tool
            .call(args(
                "foo.txt",
                vec![replacement("old", "new"), replacement("missing", "changed")],
                None,
            ))
            .await
        else {
            panic!("edit should fail");
        };

        assert!(err.to_string().contains("not found"));
        assert_eq!(fs::read_to_string(path)?, "old\nactual\n");
        Ok(())
    }

    #[tokio::test]
    async fn large_diff_uses_omitted_file_change() -> TestResult {
        let (tool, tmp) = fixture()?;
        let path = tmp.path().join("large.txt");
        fs::write(&path, "small\n")?;
        let large_line = "x".repeat(MAX_FILE_CHANGE_BYTES + 1);

        let output = tool
            .call(replacement_args("large.txt", "small", large_line))
            .await?;

        let decoded = decoded_changes(output)?;
        match &decoded.file_changes[0].change {
            FileChange::Omitted {
                operation,
                reason,
                added,
                removed,
                bytes,
            } => {
                assert_eq!(*operation, FileChangeOperation::Update);
                assert!(reason.contains("diff omitted"));
                assert_eq!(*added, 1);
                assert_eq!(*removed, 1);
                assert!(*bytes > MAX_FILE_CHANGE_BYTES);
            }
            _ => panic!("expected omitted file change"),
        }
        Ok(())
    }
}
