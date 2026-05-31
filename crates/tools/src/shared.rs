use std::path::{Path, PathBuf};

use crate::ToolError;

pub(crate) const MAX_TOOL_OUTPUT_BYTES: usize = 16 * 1024;

pub(crate) fn resolve_path_for_write(cwd: &Path, path: &str) -> Result<PathBuf, ToolError> {
    if path.is_empty() {
        return Err(ToolError::invalid_arguments("file_path must not be empty"));
    }
    let canonical_cwd = cwd.canonicalize().map_err(|err| {
        ToolError::path_resolution(format!("failed to resolve {}: {err}", cwd.display()))
    })?;
    let joined = cwd.join(path);
    let parent = joined.parent().ok_or_else(|| {
        ToolError::path_resolution(format!("path {} has no parent directory", joined.display()))
    })?;
    let canonical_parent = parent.canonicalize().map_err(|err| {
        ToolError::path_resolution(format!("failed to resolve {}: {err}", parent.display()))
    })?;
    if !canonical_parent.starts_with(&canonical_cwd) {
        return Err(ToolError::path_resolution(format!(
            "path {} escapes the workspace",
            joined.display()
        )));
    }
    let file_name = joined.file_name().ok_or_else(|| {
        ToolError::path_resolution(format!("path {} has no file name", joined.display()))
    })?;
    let resolved_path = canonical_parent.join(file_name);

    if let Ok(metadata) = resolved_path.symlink_metadata()
        && metadata.file_type().is_symlink()
    {
        let canonical_path = resolved_path.canonicalize().map_err(|err| {
            ToolError::path_resolution(format!(
                "failed to resolve {}: {err}",
                resolved_path.display()
            ))
        })?;
        if !canonical_path.starts_with(&canonical_cwd) {
            return Err(ToolError::path_resolution(format!(
                "path {} escapes the workspace",
                resolved_path.display()
            )));
        }
    }

    Ok(resolved_path)
}

pub(crate) fn truncate_output(mut output: String) -> String {
    if output.len() <= MAX_TOOL_OUTPUT_BYTES {
        return output;
    }
    output.truncate(MAX_TOOL_OUTPUT_BYTES);
    output.push_str("\n...[truncated]");
    output
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    #[cfg(unix)]
    #[test]
    fn resolve_path_for_write_rejects_symlink_target_outside_workspace()
    -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::fs::symlink;

        let workspace = TempDir::new()?;
        let outside = TempDir::new()?;
        let outside_file = outside.path().join("outside.txt");
        std::fs::write(&outside_file, "hello")?;

        symlink(&outside_file, workspace.path().join("link.txt"))?;

        let Err(err) = resolve_path_for_write(workspace.path(), "link.txt") else {
            panic!("symlink should be rejected");
        };

        assert!(err.to_string().contains("escapes the workspace"));
        Ok(())
    }
}
