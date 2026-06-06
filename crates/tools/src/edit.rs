use std::{
    collections::BTreeSet,
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
};

use rig::{completion::ToolDefinition, tool::Tool};
use schemars::{JsonSchema, schema_for};
use serde::Deserialize;
use smooth_protocol::{FileChange, FileChangeOperation, FileChangeOutput};

use crate::{
    MAX_FILE_CHANGE_BYTES, ToolError, encode_tool_output_with_file_changes,
    shared::resolve_path_for_write,
};

const DESCRIPTION: &str = r#"Update existing UTF-8 text files with structured hunks.

Call shape:
{"updates":[{"file_path":"path/to/file.rs","move_to":"optional/new/path.rs","hunks":[{"lines":[{"kind":"context","text":"old context"},{"kind":"remove","text":"old line"},{"kind":"add","text":"new line"}]}]}]}

Usage:
- `updates` must be non-empty. Each update targets an existing UTF-8 file; use `write` to create files and `delete` to remove files.
- `file_path` and optional `move_to` may be absolute or relative to the current working directory, but must resolve inside the workspace.
- A hunk is an ordered `lines` array. `context` and `remove` lines form the exact old block to match; `context` and `add` lines form the replacement block.
- Each hunk must match exactly once in the current in-memory file content. Hunks are applied sequentially within a file.
- Add-only hunks are allowed only when the target file is empty; otherwise include at least one `context` or `remove` line to locate the insertion.
- Zero hunks are allowed only when `move_to` is present.
- Updates that do not change content or move the file fail, and each source path or real move target may appear only once per call.
- All updates are prepared before any filesystem mutation. If a later apply step fails, earlier applied filesystem changes are not rolled back."#;

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
    /// Ordered file updates to prepare and then apply.
    updates: Vec<EditUpdate>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct EditUpdate {
    /// Absolute path to an existing file, or a path relative to the current working directory.
    file_path: String,

    /// Optional destination path for a rename/move. The target must not already exist unless it is
    /// the same path as file_path.
    move_to: Option<String>,

    /// Ordered hunks to apply to the file before any optional move.
    hunks: Vec<EditHunk>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct EditHunk {
    /// Ordered hunk lines. Context/remove lines must match the current content exactly once.
    lines: Vec<EditHunkLine>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct EditHunkLine {
    /// One of `context`, `remove`, or `add`.
    kind: HunkLineKind,

    /// Line content without a trailing newline.
    text: String,
}

#[derive(Debug, Clone, Copy, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum HunkLineKind {
    Context,
    Remove,
    Add,
}

impl EditArgs {
    fn into_operations(self) -> Result<Vec<PatchOperation>, ToolError> {
        if self.updates.is_empty() {
            return Err(ToolError::invalid_arguments("updates must not be empty"));
        }

        self.updates
            .into_iter()
            .enumerate()
            .map(|(index, update)| update.into_operation(index))
            .collect()
    }
}

impl EditUpdate {
    fn into_operation(self, update_index: usize) -> Result<PatchOperation, ToolError> {
        validate_update_path(
            &self.file_path,
            format!("updates[{update_index}].file_path"),
        )?;
        if let Some(move_to) = self.move_to.as_deref() {
            validate_update_path(move_to, format!("updates[{update_index}].move_to"))?;
        }
        if self.hunks.is_empty() && self.move_to.is_none() {
            return Err(ToolError::invalid_arguments(format!(
                "updates[{update_index}] must include at least one hunk or move_to"
            )));
        }

        let hunks = self
            .hunks
            .into_iter()
            .enumerate()
            .map(|(hunk_index, hunk)| hunk.into_hunk(update_index, hunk_index))
            .collect::<Result<Vec<_>, _>>()?;

        Ok(PatchOperation::Update {
            path: self.file_path,
            move_to: self.move_to,
            hunks,
        })
    }
}

impl EditHunk {
    fn into_hunk(self, update_index: usize, hunk_index: usize) -> Result<Hunk, ToolError> {
        if self.lines.is_empty() {
            return Err(ToolError::invalid_arguments(format!(
                "updates[{update_index}].hunks[{hunk_index}].lines must not be empty"
            )));
        }

        let lines = self
            .lines
            .into_iter()
            .map(|line| match line.kind {
                HunkLineKind::Context => HunkLine::Context(line.text),
                HunkLineKind::Remove => HunkLine::Remove(line.text),
                HunkLineKind::Add => HunkLine::Add(line.text),
            })
            .collect();

        Ok(Hunk { lines })
    }
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
        let operations = args.into_operations()?;
        let prepared = prepare_operations(&self.cwd, operations)?;
        let file_changes = prepared
            .iter()
            .map(PreparedChange::file_change)
            .collect::<Vec<_>>();

        for change in prepared {
            change.apply()?;
        }

        let count = file_changes.len();
        let file_label = if count == 1 { "file" } else { "files" };
        let model_output = format!("applied edits ({count} {file_label} changed)");
        Ok(encode_tool_output_with_file_changes(
            model_output,
            file_changes,
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PatchOperation {
    Update {
        path: String,
        move_to: Option<String>,
        hunks: Vec<Hunk>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Hunk {
    lines: Vec<HunkLine>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum HunkLine {
    Context(String),
    Remove(String),
    Add(String),
}

#[derive(Debug, Clone)]
enum PreparedChange {
    Update {
        path: PathBuf,
        move_to: Option<PathBuf>,
        original_content: String,
        new_content: String,
    },
}

impl PreparedChange {
    fn apply(self) -> Result<(), ToolError> {
        match self {
            Self::Update {
                path,
                move_to,
                original_content,
                new_content,
            } => {
                if original_content != new_content {
                    fs::write(&path, &new_content).map_err(|err| {
                        ToolError::io(format!("failed to write {}: {err}", path.display()))
                    })?;
                }
                if let Some(move_to) = move_to
                    && move_to != path
                {
                    fs::rename(&path, &move_to).map_err(|err| {
                        ToolError::io(format!(
                            "failed to move {} to {}: {err}",
                            path.display(),
                            move_to.display()
                        ))
                    })?;
                }
                Ok(())
            }
        }
    }

    fn file_change(&self) -> FileChangeOutput {
        match self {
            Self::Update {
                path,
                move_to,
                original_content,
                new_content,
            } => FileChangeOutput {
                path: path.clone(),
                change: update_file_change(original_content, new_content, move_to.clone()),
            },
        }
    }

    fn source_path(&self) -> &Path {
        match self {
            Self::Update { path, .. } => path,
        }
    }
}

fn validate_update_path(path: &str, label: String) -> Result<(), ToolError> {
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

fn prepare_operations(
    cwd: &Path,
    operations: Vec<PatchOperation>,
) -> Result<Vec<PreparedChange>, ToolError> {
    let mut prepared = Vec::new();
    let mut touched_paths = BTreeSet::new();

    for operation in operations {
        let change = match operation {
            PatchOperation::Update {
                path,
                move_to,
                hunks,
            } => prepare_update(cwd, &path, move_to.as_deref(), &hunks)?,
        };

        let source_path = change.source_path().to_path_buf();
        insert_unique_path(&mut touched_paths, &source_path)?;
        if let PreparedChange::Update {
            move_to: Some(move_to),
            ..
        } = &change
            && move_to != &source_path
        {
            insert_unique_path(&mut touched_paths, move_to)?;
        }
        prepared.push(change);
    }

    Ok(prepared)
}

fn prepare_update(
    cwd: &Path,
    path: &str,
    move_to: Option<&str>,
    hunks: &[Hunk],
) -> Result<PreparedChange, ToolError> {
    let path = resolve_path_for_write(cwd, path)?;
    validate_existing_file(&path, "update")?;
    let original_content = read_utf8_file(&path, "update")?;
    let new_content = apply_hunks(&path, &original_content, hunks)?;
    let move_to = move_to
        .map(|move_to| resolve_move_target(cwd, &path, move_to))
        .transpose()?
        .filter(|move_to| move_to != &path);

    if move_to.is_none() && original_content == new_content {
        return Err(ToolError::invalid_arguments(format!(
            "update file {} produced no changes",
            path.display()
        )));
    }

    Ok(PreparedChange::Update {
        path,
        move_to,
        original_content,
        new_content,
    })
}

fn validate_existing_file(path: &Path, operation: &str) -> Result<(), ToolError> {
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

fn read_utf8_file(path: &Path, operation: &str) -> Result<String, ToolError> {
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

fn resolve_move_target(cwd: &Path, source: &Path, move_to: &str) -> Result<PathBuf, ToolError> {
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

fn insert_unique_path(paths: &mut BTreeSet<PathBuf>, path: &Path) -> Result<(), ToolError> {
    if paths.insert(path.to_path_buf()) {
        return Ok(());
    }
    Err(ToolError::invalid_arguments(format!(
        "edit updates touch {} more than once; combine edits for each file into one update",
        path.display()
    )))
}

fn apply_hunks(path: &Path, content: &str, hunks: &[Hunk]) -> Result<String, ToolError> {
    if hunks.is_empty() {
        return Ok(content.to_string());
    }

    let line_ending = line_ending_for(content);
    let had_trailing_newline = content.ends_with('\n');
    let mut lines = content.lines().map(ToOwned::to_owned).collect::<Vec<_>>();
    for (index, hunk) in hunks.iter().enumerate() {
        apply_hunk(path, &mut lines, hunk, index)?;
    }
    Ok(join_lines(&lines, line_ending, had_trailing_newline))
}

fn apply_hunk(
    path: &Path,
    lines: &mut Vec<String>,
    hunk: &Hunk,
    hunk_index: usize,
) -> Result<(), ToolError> {
    let old_lines = hunk
        .lines
        .iter()
        .filter_map(|line| match line {
            HunkLine::Context(content) | HunkLine::Remove(content) => Some(content.as_str()),
            HunkLine::Add(_) => None,
        })
        .collect::<Vec<_>>();
    let new_lines = hunk
        .lines
        .iter()
        .filter_map(|line| match line {
            HunkLine::Context(content) | HunkLine::Add(content) => Some(content.clone()),
            HunkLine::Remove(_) => None,
        })
        .collect::<Vec<_>>();

    if old_lines.is_empty() && !lines.is_empty() {
        return Err(ToolError::invalid_arguments(format!(
            "hunk {hunk_index} in {} is add-only; add-only hunks need a context or remove line to locate the insertion unless the file is empty",
            path.display()
        )));
    }

    let matches = find_hunk_matches(lines, &old_lines);
    match matches.as_slice() {
        [] => Err(ToolError::invalid_arguments(format!(
            "hunk {hunk_index} context not found in {}",
            path.display()
        ))),
        [start] => {
            lines.splice(*start..(*start + old_lines.len()), new_lines);
            Ok(())
        }
        _ => Err(ToolError::invalid_arguments(format!(
            "hunk {hunk_index} context is ambiguous in {} ({} matches)",
            path.display(),
            matches.len()
        ))),
    }
}

fn find_hunk_matches(lines: &[String], old_lines: &[&str]) -> Vec<usize> {
    if old_lines.is_empty() {
        if lines.is_empty() {
            return vec![0];
        }
        return (0..=lines.len()).collect();
    }

    if old_lines.len() > lines.len() {
        return Vec::new();
    }

    (0..=(lines.len() - old_lines.len()))
        .filter(|start| {
            old_lines
                .iter()
                .enumerate()
                .all(|(offset, old_line)| lines[start + offset] == *old_line)
        })
        .collect()
}

fn line_ending_for(content: &str) -> &'static str {
    if content.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

fn join_lines(lines: &[String], line_ending: &str, trailing_newline: bool) -> String {
    if lines.is_empty() {
        return String::new();
    }
    let mut content = lines.join(line_ending);
    if trailing_newline {
        content.push_str(line_ending);
    }
    content
}

fn update_file_change(
    original_content: &str,
    new_content: &str,
    move_path: Option<PathBuf>,
) -> FileChange {
    let unified_diff = if original_content == new_content {
        String::new()
    } else {
        diffy::create_patch(original_content, new_content).to_string()
    };
    let (added, removed) = diff_line_counts(&unified_diff);
    if unified_diff.len() > MAX_FILE_CHANGE_BYTES {
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

    fn args(updates: Vec<EditUpdate>) -> EditArgs {
        EditArgs { updates }
    }

    fn update(
        file_path: impl Into<String>,
        move_to: Option<&str>,
        hunks: Vec<EditHunk>,
    ) -> EditUpdate {
        EditUpdate {
            file_path: file_path.into(),
            move_to: move_to.map(str::to_string),
            hunks,
        }
    }

    fn hunk(lines: Vec<EditHunkLine>) -> EditHunk {
        EditHunk { lines }
    }

    fn context(text: impl Into<String>) -> EditHunkLine {
        EditHunkLine {
            kind: HunkLineKind::Context,
            text: text.into(),
        }
    }

    fn remove(text: impl Into<String>) -> EditHunkLine {
        EditHunkLine {
            kind: HunkLineKind::Remove,
            text: text.into(),
        }
    }

    fn add(text: impl Into<String>) -> EditHunkLine {
        EditHunkLine {
            kind: HunkLineKind::Add,
            text: text.into(),
        }
    }

    fn replacement_hunk(old: impl Into<String>, new: impl Into<String>) -> EditHunk {
        hunk(vec![remove(old), add(new)])
    }

    fn replacement_args(
        file_path: impl Into<String>,
        old: impl Into<String>,
        new: impl Into<String>,
    ) -> EditArgs {
        args(vec![update(
            file_path,
            None,
            vec![replacement_hunk(old, new)],
        )])
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
    fn deserializes_structured_args() -> TestResult {
        let args: EditArgs = serde_json::from_value(serde_json::json!({
            "updates": [
                {
                    "file_path": "foo.txt",
                    "move_to": "bar.txt",
                    "hunks": [
                        {
                            "lines": [
                                { "kind": "context", "text": "before" },
                                { "kind": "remove", "text": "old" },
                                { "kind": "add", "text": "new" }
                            ]
                        }
                    ]
                }
            ]
        }))?;

        assert_eq!(args.updates.len(), 1);
        assert_eq!(args.updates[0].file_path, "foo.txt");
        assert_eq!(args.updates[0].move_to.as_deref(), Some("bar.txt"));
        assert_eq!(
            args.updates[0].hunks[0].lines[0].kind,
            HunkLineKind::Context
        );
        Ok(())
    }

    #[test]
    fn rejects_old_patch_args() {
        let message = deserialize_error(serde_json::json!({
            "patch": "*** Begin Patch\n*** Update File: foo.txt\n@@\n-old\n+new\n*** End Patch\n"
        }));

        assert!(
            message.contains("missing field `updates`") || message.contains("unknown field"),
            "{message}"
        );
    }

    #[test]
    fn rejects_old_replacement_args() {
        let message = deserialize_error(serde_json::json!({
            "file_path": "foo.txt",
            "replacements": [
                {
                    "old_string": "world",
                    "new_string": "there"
                }
            ]
        }));

        assert!(
            message.contains("missing field `updates`") || message.contains("unknown field"),
            "{message}"
        );
    }

    #[test]
    fn rejects_unknown_args() {
        for value in [
            serde_json::json!({
                "updates": [],
                "unexpected": true
            }),
            serde_json::json!({
                "updates": [
                    {
                        "file_path": "foo.txt",
                        "hunks": [],
                        "unexpected": true
                    }
                ]
            }),
            serde_json::json!({
                "updates": [
                    {
                        "file_path": "foo.txt",
                        "hunks": [
                            {
                                "lines": [],
                                "unexpected": true
                            }
                        ]
                    }
                ]
            }),
            serde_json::json!({
                "updates": [
                    {
                        "file_path": "foo.txt",
                        "hunks": [
                            {
                                "lines": [
                                    {
                                        "kind": "context",
                                        "text": "old",
                                        "unexpected": true
                                    }
                                ]
                            }
                        ]
                    }
                ]
            }),
        ] {
            let message = deserialize_error(value);
            assert!(message.contains("unknown field"), "{message}");
        }
    }

    #[test]
    fn rejects_invalid_hunk_kind() {
        let message = deserialize_error(serde_json::json!({
            "updates": [
                {
                    "file_path": "foo.txt",
                    "hunks": [
                        {
                            "lines": [
                                { "kind": "replace", "text": "old" }
                            ]
                        }
                    ]
                }
            ]
        }));

        assert!(message.contains("unknown variant"), "{message}");
    }

    #[tokio::test]
    async fn applies_single_file_update() -> TestResult {
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
    async fn applies_move_update() -> TestResult {
        let (tool, tmp) = fixture()?;
        let source = tmp.path().join("old.txt");
        let target = tmp.path().join("new.txt");
        fs::write(&source, "before\n")?;

        let output = tool
            .call(args(vec![update(
                "old.txt",
                Some("new.txt"),
                vec![replacement_hunk("before", "after")],
            )]))
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
    async fn applies_rename_only_update_without_hunks() -> TestResult {
        let (tool, tmp) = fixture()?;
        let source = tmp.path().join("old.txt");
        let target = tmp.path().join("new.txt");
        fs::write(&source, "same\n")?;

        let output = tool
            .call(args(vec![update("old.txt", Some("new.txt"), Vec::new())]))
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
    async fn accepts_move_to_equal_file_path() -> TestResult {
        let (tool, tmp) = fixture()?;
        let path = tmp.path().join("same.txt");
        fs::write(&path, "before\n")?;

        let output = tool
            .call(args(vec![update(
                "same.txt",
                Some("same.txt"),
                vec![replacement_hunk("before", "after")],
            )]))
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
            .call(args(vec![update("same.txt", Some("same.txt"), Vec::new())]))
            .await
        else {
            panic!("edit should fail");
        };

        assert!(err.to_string().contains("produced no changes"));
        assert_eq!(fs::read_to_string(path)?, "before\n");
        Ok(())
    }

    #[tokio::test]
    async fn applies_multi_file_update() -> TestResult {
        let (tool, tmp) = fixture()?;
        fs::write(tmp.path().join("one.txt"), "one\n")?;
        fs::write(tmp.path().join("two.txt"), "two\n")?;

        let output = tool
            .call(args(vec![
                update("one.txt", None, vec![replacement_hunk("one", "uno")]),
                update("two.txt", None, vec![replacement_hunk("two", "dos")]),
            ]))
            .await?;

        assert_eq!(fs::read_to_string(tmp.path().join("one.txt"))?, "uno\n");
        assert_eq!(fs::read_to_string(tmp.path().join("two.txt"))?, "dos\n");
        let decoded = decoded_changes(output)?;
        assert_eq!(decoded.model_output, "applied edits (2 files changed)");
        assert_eq!(decoded.file_changes.len(), 2);
        assert!(matches!(
            decoded.file_changes[0].change,
            FileChange::Update { .. }
        ));
        assert!(matches!(
            decoded.file_changes[1].change,
            FileChange::Update { .. }
        ));
        Ok(())
    }

    #[tokio::test]
    async fn preserves_crlf_for_update_insertions() -> TestResult {
        let (tool, tmp) = fixture()?;
        let path = tmp.path().join("crlf.txt");
        fs::write(&path, "alpha\r\nbeta\r\n")?;

        tool.call(args(vec![update(
            "crlf.txt",
            None,
            vec![hunk(vec![
                context("alpha"),
                add("inserted"),
                context("beta"),
            ])],
        )]))
        .await?;

        assert_eq!(fs::read_to_string(path)?, "alpha\r\ninserted\r\nbeta\r\n");
        Ok(())
    }

    #[tokio::test]
    async fn applies_blank_context_line() -> TestResult {
        let (tool, tmp) = fixture()?;
        let path = tmp.path().join("blank.txt");
        fs::write(&path, "alpha\n\nbeta\n")?;

        tool.call(args(vec![update(
            "blank.txt",
            None,
            vec![hunk(vec![
                context("alpha"),
                context(""),
                add("inserted"),
                context("beta"),
            ])],
        )]))
        .await?;

        assert_eq!(fs::read_to_string(path)?, "alpha\n\ninserted\nbeta\n");
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
    async fn rejects_missing_hunk_context() -> TestResult {
        let (tool, tmp) = fixture()?;
        fs::write(tmp.path().join("foo.txt"), "actual\n")?;

        let Err(err) = tool.call(replacement_args("foo.txt", "old", "new")).await else {
            panic!("edit should fail");
        };

        assert!(err.to_string().contains("context not found"));
        assert_eq!(fs::read_to_string(tmp.path().join("foo.txt"))?, "actual\n");
        Ok(())
    }

    #[tokio::test]
    async fn rejects_ambiguous_hunk_context() -> TestResult {
        let (tool, tmp) = fixture()?;
        fs::write(tmp.path().join("foo.txt"), "same\nsame\n")?;

        let Err(err) = tool
            .call(replacement_args("foo.txt", "same", "changed"))
            .await
        else {
            panic!("edit should fail");
        };

        assert!(err.to_string().contains("context is ambiguous"));
        assert_eq!(
            fs::read_to_string(tmp.path().join("foo.txt"))?,
            "same\nsame\n"
        );
        Ok(())
    }

    #[tokio::test]
    async fn rejects_add_only_hunk_on_non_empty_file_with_targeted_error() -> TestResult {
        let (tool, tmp) = fixture()?;
        fs::write(tmp.path().join("foo.txt"), "actual\n")?;

        let Err(err) = tool
            .call(args(vec![update(
                "foo.txt",
                None,
                vec![hunk(vec![add("inserted")])],
            )]))
            .await
        else {
            panic!("edit should fail");
        };

        assert!(err.to_string().contains("is add-only"));
        assert!(err.to_string().contains("unless the file is empty"));
        assert_eq!(fs::read_to_string(tmp.path().join("foo.txt"))?, "actual\n");
        Ok(())
    }

    #[tokio::test]
    async fn applies_add_only_hunk_to_empty_file() -> TestResult {
        let (tool, tmp) = fixture()?;
        let path = tmp.path().join("empty.txt");
        fs::write(&path, "")?;

        tool.call(args(vec![update(
            "empty.txt",
            None,
            vec![hunk(vec![add("inserted")])],
        )]))
        .await?;

        assert_eq!(fs::read_to_string(path)?, "inserted");
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
    async fn rejects_non_utf8_update_input() -> TestResult {
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
    async fn rejects_empty_updates() -> TestResult {
        let (tool, _tmp) = fixture()?;

        let Err(err) = tool.call(args(Vec::new())).await else {
            panic!("edit should fail");
        };

        assert!(err.to_string().contains("updates must not be empty"));
        Ok(())
    }

    #[tokio::test]
    async fn rejects_update_without_hunks_or_move() -> TestResult {
        let (tool, tmp) = fixture()?;
        fs::write(tmp.path().join("foo.txt"), "hello\n")?;

        let Err(err) = tool
            .call(args(vec![update("foo.txt", None, Vec::new())]))
            .await
        else {
            panic!("edit should fail");
        };

        assert!(err.to_string().contains("hunk or move_to"));
        Ok(())
    }

    #[tokio::test]
    async fn rejects_empty_hunk() -> TestResult {
        let (tool, tmp) = fixture()?;
        fs::write(tmp.path().join("foo.txt"), "hello\n")?;

        let Err(err) = tool
            .call(args(vec![update("foo.txt", None, vec![hunk(Vec::new())])]))
            .await
        else {
            panic!("edit should fail");
        };

        assert!(err.to_string().contains("lines must not be empty"));
        Ok(())
    }

    #[tokio::test]
    async fn rejects_no_op_update() -> TestResult {
        let (tool, tmp) = fixture()?;
        fs::write(tmp.path().join("foo.txt"), "hello\n")?;

        let Err(err) = tool
            .call(args(vec![update(
                "foo.txt",
                None,
                vec![hunk(vec![context("hello")])],
            )]))
            .await
        else {
            panic!("edit should fail");
        };

        assert!(err.to_string().contains("produced no changes"));
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
    async fn rejects_duplicate_file_path() -> TestResult {
        let (tool, tmp) = fixture()?;
        let path = tmp.path().join("foo.txt");
        fs::write(&path, "old\n")?;

        let Err(err) = tool
            .call(args(vec![
                update("foo.txt", None, vec![replacement_hunk("old", "new")]),
                update("foo.txt", None, vec![replacement_hunk("old", "newer")]),
            ]))
            .await
        else {
            panic!("edit should fail");
        };

        assert!(err.to_string().contains("more than once"));
        assert_eq!(fs::read_to_string(path)?, "old\n");
        Ok(())
    }

    #[tokio::test]
    async fn rejects_move_target_collision() -> TestResult {
        let (tool, tmp) = fixture()?;
        let first = tmp.path().join("first.txt");
        let second = tmp.path().join("second.txt");
        fs::write(&first, "first\n")?;
        fs::write(&second, "second\n")?;

        let Err(err) = tool
            .call(args(vec![
                update("first.txt", Some("moved.txt"), Vec::new()),
                update("second.txt", Some("moved.txt"), Vec::new()),
            ]))
            .await
        else {
            panic!("edit should fail");
        };

        assert!(err.to_string().contains("more than once"));
        assert_eq!(fs::read_to_string(first)?, "first\n");
        assert_eq!(fs::read_to_string(second)?, "second\n");
        assert!(!tmp.path().join("moved.txt").exists());
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
            .call(args(vec![update(
                "source.txt",
                Some("target.txt"),
                Vec::new(),
            )]))
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
    async fn rolls_back_when_later_hunk_fails() -> TestResult {
        let (tool, tmp) = fixture()?;
        let first = tmp.path().join("first.txt");
        let second = tmp.path().join("second.txt");
        fs::write(&first, "old\n")?;
        fs::write(&second, "actual\n")?;

        let Err(err) = tool
            .call(args(vec![
                update("first.txt", None, vec![replacement_hunk("old", "new")]),
                update(
                    "second.txt",
                    None,
                    vec![replacement_hunk("missing", "changed")],
                ),
            ]))
            .await
        else {
            panic!("edit should fail");
        };

        assert!(err.to_string().contains("context not found"));
        assert_eq!(fs::read_to_string(first)?, "old\n");
        assert_eq!(fs::read_to_string(second)?, "actual\n");
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
