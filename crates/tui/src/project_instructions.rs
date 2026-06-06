use std::{
    fs,
    io::ErrorKind,
    path::{Path, PathBuf},
};

use smooth_protocol::{ProjectInstructionEntry, ProjectInstructions};

use crate::error::TuiResult;

const PROJECT_INSTRUCTIONS_MAX_BYTES: usize = 32 * 1024;

pub(crate) fn load_project_instructions() -> TuiResult<Option<ProjectInstructions>> {
    let cwd = std::env::current_dir()?;
    Ok(load_project_instructions_from(&cwd))
}

pub(crate) fn load_project_instructions_from(start_dir: &Path) -> Option<ProjectInstructions> {
    let dirs = instruction_search_dirs(start_dir);
    let mut entries = Vec::new();
    let mut total_bytes = 0usize;

    for dir in dirs {
        if total_bytes >= PROJECT_INSTRUCTIONS_MAX_BYTES {
            break;
        }
        let Some(source_path) = instruction_file_for_dir(&dir) else {
            continue;
        };
        let Some(text) = read_instruction_text(&source_path) else {
            continue;
        };
        if text.trim().is_empty() {
            continue;
        }

        let remaining = PROJECT_INSTRUCTIONS_MAX_BYTES.saturating_sub(total_bytes);
        let (text, truncated) = truncate_to_byte_limit(&text, remaining);
        if truncated {
            tracing::warn!(
                source_path = %source_path.display(),
                max_bytes = PROJECT_INSTRUCTIONS_MAX_BYTES,
                "project instructions exceeded total byte cap and were truncated"
            );
        }
        if text.is_empty() {
            continue;
        }
        total_bytes = total_bytes.saturating_add(text.len());
        entries.push(ProjectInstructionEntry {
            source_path: source_path.display().to_string(),
            directory: dir.display().to_string(),
            text,
        });
    }

    (!entries.is_empty()).then_some(ProjectInstructions { entries })
}

fn instruction_search_dirs(start_dir: &Path) -> Vec<PathBuf> {
    let Some(git_root) = nearest_git_root(start_dir) else {
        return vec![start_dir.to_path_buf()];
    };

    let mut dirs = Vec::new();
    for dir in start_dir.ancestors() {
        dirs.push(dir.to_path_buf());
        if dir == git_root {
            break;
        }
    }
    dirs.reverse();
    dirs
}

fn nearest_git_root(start_dir: &Path) -> Option<PathBuf> {
    start_dir
        .ancestors()
        .find(|dir| dir.join(".git").exists())
        .map(Path::to_path_buf)
}

fn instruction_file_for_dir(dir: &Path) -> Option<PathBuf> {
    let override_path = dir.join("AGENTS.override.md");
    if is_regular_file(&override_path) {
        return Some(override_path);
    }

    let agents_path = dir.join("AGENTS.md");
    is_regular_file(&agents_path).then_some(agents_path)
}

fn is_regular_file(path: &Path) -> bool {
    match fs::metadata(path) {
        Ok(metadata) => metadata.is_file(),
        Err(err) if err.kind() == ErrorKind::NotFound => false,
        Err(err) => {
            tracing::warn!(
                source_path = %path.display(),
                error = %err,
                "failed to inspect project instructions file"
            );
            false
        }
    }
}

fn read_instruction_text(path: &Path) -> Option<String> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(
                source_path = %path.display(),
                error = %err,
                "failed to read project instructions file"
            );
            return None;
        }
    };

    match String::from_utf8(bytes) {
        Ok(text) => Some(text),
        Err(err) => {
            let bytes = err.into_bytes();
            tracing::warn!(
                source_path = %path.display(),
                "project instructions file was not valid UTF-8; decoded lossily"
            );
            Some(String::from_utf8_lossy(&bytes).into_owned())
        }
    }
}

fn truncate_to_byte_limit(text: &str, limit: usize) -> (String, bool) {
    if text.len() <= limit {
        return (text.to_string(), false);
    }

    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use anyhow::Result;
    use tempfile::TempDir;

    use super::{PROJECT_INSTRUCTIONS_MAX_BYTES, load_project_instructions_from};

    #[test]
    fn root_agents_file_loads() -> Result<()> {
        let temp = TempDir::new()?;
        fs::create_dir(temp.path().join(".git"))?;
        fs::write(temp.path().join("AGENTS.md"), "root instructions")?;

        let instructions = load_project_instructions_from(temp.path())
            .ok_or_else(|| anyhow::anyhow!("expected project instructions"))?;

        assert_eq!(instructions.entries.len(), 1);
        assert_eq!(instructions.entries[0].text, "root instructions");
        assert_eq!(
            instructions.entries[0].source_path,
            temp.path().join("AGENTS.md").display().to_string()
        );
        Ok(())
    }

    #[test]
    fn nested_agents_load_root_to_cwd() -> Result<()> {
        let temp = TempDir::new()?;
        let nested = temp.path().join("crates").join("tui");
        fs::create_dir(temp.path().join(".git"))?;
        fs::create_dir_all(&nested)?;
        fs::write(temp.path().join("AGENTS.md"), "root")?;
        fs::write(temp.path().join("crates").join("AGENTS.md"), "crates")?;
        fs::write(nested.join("AGENTS.md"), "tui")?;

        let instructions = load_project_instructions_from(&nested)
            .ok_or_else(|| anyhow::anyhow!("expected project instructions"))?;
        let texts = instructions
            .entries
            .iter()
            .map(|entry| entry.text.as_str())
            .collect::<Vec<_>>();

        assert_eq!(texts, vec!["root", "crates", "tui"]);
        Ok(())
    }

    #[test]
    fn agents_override_wins() -> Result<()> {
        let temp = TempDir::new()?;
        fs::create_dir(temp.path().join(".git"))?;
        fs::write(temp.path().join("AGENTS.md"), "base")?;
        fs::write(temp.path().join("AGENTS.override.md"), "override")?;

        let instructions = load_project_instructions_from(temp.path())
            .ok_or_else(|| anyhow::anyhow!("expected project instructions"))?;

        assert_eq!(instructions.entries.len(), 1);
        assert_eq!(instructions.entries[0].text, "override");
        assert!(
            instructions.entries[0]
                .source_path
                .ends_with("AGENTS.override.md")
        );
        Ok(())
    }

    #[test]
    fn missing_agents_returns_none() -> Result<()> {
        let temp = TempDir::new()?;
        fs::create_dir(temp.path().join(".git"))?;

        assert!(load_project_instructions_from(temp.path()).is_none());
        Ok(())
    }

    #[test]
    fn total_cap_truncates_on_char_boundaries() -> Result<()> {
        let temp = TempDir::new()?;
        fs::create_dir(temp.path().join(".git"))?;
        let text = format!("{}étail", "a".repeat(PROJECT_INSTRUCTIONS_MAX_BYTES - 1));
        fs::write(temp.path().join("AGENTS.md"), text)?;

        let instructions = load_project_instructions_from(temp.path())
            .ok_or_else(|| anyhow::anyhow!("expected project instructions"))?;
        let loaded = &instructions.entries[0].text;

        assert_eq!(loaded.len(), PROJECT_INSTRUCTIONS_MAX_BYTES - 1);
        assert!(loaded.is_char_boundary(loaded.len()));
        assert!(loaded.chars().all(|ch| ch == 'a'));
        Ok(())
    }
}
