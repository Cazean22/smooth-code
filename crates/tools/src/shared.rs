use std::path::{Path, PathBuf};

use crate::ToolFailure;

pub(crate) const MAX_TOOL_OUTPUT_BYTES: usize = 16 * 1024;

pub(crate) fn resolve_path(cwd: &Path, path: Option<&str>) -> Result<PathBuf, ToolFailure> {
    let path = match path {
        Some(path) if !path.is_empty() => cwd.join(path),
        _ => cwd.to_path_buf(),
    };
    let canonical = path
        .canonicalize()
        .map_err(|err| ToolFailure::new(format!("failed to resolve {}: {err}", path.display())))?;
    if !canonical.starts_with(cwd) {
        return Err(ToolFailure::new(format!(
            "path {} escapes the workspace",
            canonical.display()
        )));
    }
    Ok(canonical)
}

pub(crate) fn resolve_path_for_write(cwd: &Path, path: &str) -> Result<PathBuf, ToolFailure> {
    if path.is_empty() {
        return Err(ToolFailure::new("file_path must not be empty"));
    }
    let canonical_cwd = cwd
        .canonicalize()
        .map_err(|err| ToolFailure::new(format!("failed to resolve {}: {err}", cwd.display())))?;
    let joined = cwd.join(path);
    let parent = joined.parent().ok_or_else(|| {
        ToolFailure::new(format!("path {} has no parent directory", joined.display()))
    })?;
    let canonical_parent = parent.canonicalize().map_err(|err| {
        ToolFailure::new(format!("failed to resolve {}: {err}", parent.display()))
    })?;
    if !canonical_parent.starts_with(&canonical_cwd) {
        return Err(ToolFailure::new(format!(
            "path {} escapes the workspace",
            joined.display()
        )));
    }
    let file_name = joined
        .file_name()
        .ok_or_else(|| ToolFailure::new(format!("path {} has no file name", joined.display())))?;
    Ok(canonical_parent.join(file_name))
}

pub(crate) fn truncate_output(mut output: String) -> String {
    if output.len() <= MAX_TOOL_OUTPUT_BYTES {
        return output;
    }
    output.truncate(MAX_TOOL_OUTPUT_BYTES);
    output.push_str("\n...[truncated]");
    output
}
