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

pub(crate) fn truncate_output(mut output: String) -> String {
    if output.len() <= MAX_TOOL_OUTPUT_BYTES {
        return output;
    }
    output.truncate(MAX_TOOL_OUTPUT_BYTES);
    output.push_str("\n...[truncated]");
    output
}
