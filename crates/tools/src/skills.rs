use std::path::{Path, PathBuf};

/// Maximum bytes of a SKILL.md file that are read; longer bodies are truncated.
/// This is the built-in default; callers thread a configured value through.
pub(crate) const MAX_SKILL_BYTES: usize = 64 * 1024;
/// Maximum length of a skill name (the skill's directory name).
const MAX_NAME_LEN: usize = 64;

/// Metadata for a discovered skill (frontmatter only, body not loaded).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillMeta {
    /// Canonical skill name; always equals the skill's directory name.
    pub name: String,
    /// One-line description from the SKILL.md frontmatter.
    pub description: String,
    /// Path to the SKILL.md file.
    pub path: PathBuf,
}

/// A fully loaded skill: metadata plus the markdown body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub meta: SkillMeta,
    pub body: String,
}

/// Directory that holds project skills: `<cwd>/.smooth-code/skills`.
pub fn skills_dir(cwd: &Path) -> PathBuf {
    cwd.join(".smooth-code").join("skills")
}

fn is_valid_skill_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// List all valid skills under `<cwd>/.smooth-code/skills`, sorted by name.
///
/// Entries that are not directories, have invalid names, lack a SKILL.md, or
/// fail frontmatter validation are skipped with a warning rather than
/// surfacing an error: a single malformed skill must not break discovery.
pub fn list_skills(cwd: &Path, max_skill_bytes: usize) -> Vec<SkillMeta> {
    let dir = skills_dir(cwd);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut skills = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !is_valid_skill_name(name) {
            tracing::warn!(skill = %name, "skipping skill with invalid name");
            continue;
        }
        match load_skill_at(&path.join("SKILL.md"), name, max_skill_bytes) {
            Some(skill) => skills.push(skill.meta),
            None => {
                tracing::warn!(skill = %name, "skipping skill with missing or invalid SKILL.md");
            }
        }
    }
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
}

/// Load a single skill by name. Returns `None` if the name is invalid or the
/// skill does not exist / fails validation.
pub fn load_skill(cwd: &Path, name: &str, max_skill_bytes: usize) -> Option<Skill> {
    if !is_valid_skill_name(name) {
        return None;
    }
    load_skill_at(
        &skills_dir(cwd).join(name).join("SKILL.md"),
        name,
        max_skill_bytes,
    )
}

fn load_skill_at(path: &Path, dir_name: &str, max_skill_bytes: usize) -> Option<Skill> {
    let raw = read_bounded_utf8(path, dir_name, max_skill_bytes)?;
    let (frontmatter_name, description, body) = parse_skill_md(&raw)?;
    if let Some(frontmatter_name) = frontmatter_name
        && frontmatter_name != dir_name
    {
        tracing::warn!(
            skill = %dir_name,
            frontmatter_name = %frontmatter_name,
            "frontmatter name does not match skill directory name"
        );
        return None;
    }
    Some(Skill {
        meta: SkillMeta {
            name: dir_name.to_string(),
            description,
            path: path.to_path_buf(),
        },
        body,
    })
}

/// Read at most `MAX_SKILL_BYTES` of `path`, without ever pulling more than
/// that into memory: a pathologically large SKILL.md must not stall callers
/// (the TUI popup loads skills synchronously from key handling).
///
/// Returns `None` on IO errors or invalid UTF-8 — except a multi-byte
/// character split by the cap itself, which is dropped to keep the valid
/// prefix.
fn read_bounded_utf8(path: &Path, dir_name: &str, max_skill_bytes: usize) -> Option<String> {
    use std::io::Read;

    let file = std::fs::File::open(path).ok()?;
    let mut raw = Vec::new();
    file.take(max_skill_bytes as u64 + 1)
        .read_to_end(&mut raw)
        .ok()?;
    let truncated = raw.len() > max_skill_bytes;
    if truncated {
        tracing::warn!(skill = %dir_name, "SKILL.md exceeds {max_skill_bytes} bytes; truncating");
        raw.truncate(max_skill_bytes);
    }
    match String::from_utf8(raw) {
        Ok(text) => Some(text),
        // `error_len() == None` means the bytes end mid-character; that is
        // only legitimate when the cap did the cutting. Anything else is a
        // genuinely invalid file and is rejected like `read_to_string` did.
        Err(err) if truncated && err.utf8_error().error_len().is_none() => {
            let valid = err.utf8_error().valid_up_to();
            let mut raw = err.into_bytes();
            raw.truncate(valid);
            String::from_utf8(raw).ok()
        }
        Err(_) => None,
    }
}

/// Render the model-facing expansion of a skill invocation, shared by the
/// `/name` slash path and the `skill` tool.
pub fn render_skill_invocation(skill: &Skill, args: Option<&str>) -> String {
    let name = &skill.meta.name;
    let body = skill.body.trim();
    let args = match args.map(str::trim) {
        Some(args) if !args.is_empty() => args,
        _ => "(no additional arguments)",
    };
    format!(
        "<skill-invocation skill=\"{name}\">\nThe user invoked the \"/{name}\" skill. Follow the instructions below for this request.\n\n{body}\n</skill-invocation>\n\n{args}"
    )
}

/// Parse a SKILL.md: a `---`-delimited frontmatter block of single-line
/// `key: value` pairs followed by the markdown body. Returns
/// `(frontmatter name, description, body)`; `description` is required.
///
/// This is intentionally not a YAML parser: values may be optionally single-
/// or double-quoted, unknown keys and `#` comments are ignored, and multiline
/// values are unsupported.
fn parse_skill_md(text: &str) -> Option<(Option<String>, String, String)> {
    let rest = text.strip_prefix("---")?;
    let rest = rest
        .strip_prefix("\r\n")
        .or_else(|| rest.strip_prefix('\n'))?;
    let (frontmatter, body) = match rest.split_once("\n---") {
        Some((frontmatter, after)) => {
            // The closing fence must end its line.
            let body = after
                .strip_prefix("\r\n")
                .or_else(|| after.strip_prefix('\n'))
                .or(if after.is_empty() { Some("") } else { None })?;
            (frontmatter, body)
        }
        None => return None,
    };

    let mut name = None;
    let mut description = None;
    for line in frontmatter.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = unquote(value.trim());
        match key.trim() {
            "name" => name = Some(value.to_string()),
            "description" => description = Some(value.to_string()),
            _ => {}
        }
    }

    let description = description?;
    if description.is_empty() {
        return None;
    }
    Some((name, description, body.to_string()))
}

fn unquote(value: &str) -> &str {
    let bytes = value.as_bytes();
    if bytes.len() >= 2 {
        let (first, last) = (bytes[0], bytes[bytes.len() - 1]);
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &value[1..value.len() - 1];
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn write_skill(root: &Path, name: &str, content: &str) -> TestResult {
        let dir = skills_dir(root).join(name);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("SKILL.md"), content)?;
        Ok(())
    }

    #[test]
    fn parses_frontmatter_and_body() {
        let parsed =
            parse_skill_md("---\nname: deploy\ndescription: Deploy the app\n---\nDo the deploy.\n");
        let Some((name, description, body)) = parsed else {
            panic!("expected parse to succeed");
        };
        assert_eq!(name.as_deref(), Some("deploy"));
        assert_eq!(description, "Deploy the app");
        assert_eq!(body, "Do the deploy.\n");
    }

    #[test]
    fn parses_quoted_values_and_skips_comments_and_unknown_keys() {
        let parsed = parse_skill_md(
            "---\n# a comment\nname: \"deploy\"\ndescription: 'Deploy: now'\nextra: ignored\n---\nbody",
        );
        let Some((name, description, body)) = parsed else {
            panic!("expected parse to succeed");
        };
        assert_eq!(name.as_deref(), Some("deploy"));
        assert_eq!(description, "Deploy: now");
        assert_eq!(body, "body");
    }

    #[test]
    fn rejects_missing_frontmatter_fence() {
        assert!(parse_skill_md("name: x\ndescription: y\nbody").is_none());
        assert!(parse_skill_md("---\ndescription: y\nno closing fence").is_none());
    }

    #[test]
    fn rejects_missing_or_empty_description() {
        assert!(parse_skill_md("---\nname: x\n---\nbody").is_none());
        assert!(parse_skill_md("---\ndescription:\n---\nbody").is_none());
    }

    #[test]
    fn name_is_optional_in_frontmatter() {
        let parsed = parse_skill_md("---\ndescription: Just a description\n---\nbody");
        let Some((name, description, _)) = parsed else {
            panic!("expected parse to succeed");
        };
        assert!(name.is_none());
        assert_eq!(description, "Just a description");
    }

    #[test]
    fn list_skills_returns_empty_for_missing_dir() {
        let temp = tempfile::TempDir::new().ok();
        let Some(temp) = temp else {
            panic!("tempdir creation failed");
        };
        assert!(list_skills(temp.path(), MAX_SKILL_BYTES).is_empty());
    }

    #[test]
    fn list_skills_finds_and_sorts_valid_skills() -> TestResult {
        let temp = tempfile::TempDir::new()?;
        write_skill(
            temp.path(),
            "zeta",
            "---\ndescription: Z skill\n---\nz body",
        )?;
        write_skill(
            temp.path(),
            "alpha",
            "---\ndescription: A skill\n---\na body",
        )?;
        // Invalid: no description.
        write_skill(temp.path(), "broken", "---\nname: broken\n---\nbody")?;
        // Invalid: name charset.
        write_skill(temp.path(), "bad name", "---\ndescription: nope\n---\nbody")?;

        let skills = list_skills(temp.path(), MAX_SKILL_BYTES);
        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "zeta"]);
        assert_eq!(skills[0].description, "A skill");
        Ok(())
    }

    #[test]
    fn load_skill_returns_body() -> TestResult {
        let temp = tempfile::TempDir::new()?;
        write_skill(
            temp.path(),
            "deploy",
            "---\nname: deploy\ndescription: Deploy the app\n---\nRun make deploy.\n",
        )?;

        let Some(skill) = load_skill(temp.path(), "deploy", MAX_SKILL_BYTES) else {
            panic!("expected skill to load");
        };
        assert_eq!(skill.meta.name, "deploy");
        assert_eq!(skill.meta.description, "Deploy the app");
        assert_eq!(skill.body, "Run make deploy.\n");
        Ok(())
    }

    #[test]
    fn load_skill_rejects_frontmatter_name_mismatch() -> TestResult {
        let temp = tempfile::TempDir::new()?;
        write_skill(
            temp.path(),
            "deploy",
            "---\nname: other\ndescription: Mismatch\n---\nbody",
        )?;
        assert!(load_skill(temp.path(), "deploy", MAX_SKILL_BYTES).is_none());
        Ok(())
    }

    #[test]
    fn load_skill_rejects_invalid_names() {
        let temp = tempfile::TempDir::new().ok();
        let Some(temp) = temp else {
            panic!("tempdir creation failed");
        };
        assert!(load_skill(temp.path(), "../escape", MAX_SKILL_BYTES).is_none());
        assert!(load_skill(temp.path(), "", MAX_SKILL_BYTES).is_none());
        assert!(load_skill(temp.path(), &"x".repeat(65), MAX_SKILL_BYTES).is_none());
    }

    #[test]
    fn oversized_skill_md_is_truncated() -> TestResult {
        let temp = tempfile::TempDir::new()?;
        let header = "---\ndescription: Big skill\n---\n";
        let body = "x".repeat(MAX_SKILL_BYTES * 2);
        write_skill(temp.path(), "big", &format!("{header}{body}"))?;

        let Some(skill) = load_skill(temp.path(), "big", MAX_SKILL_BYTES) else {
            panic!("expected oversized skill to load truncated");
        };
        assert!(skill.body.len() <= MAX_SKILL_BYTES);
        assert!(skill.body.starts_with('x'));
        Ok(())
    }

    #[test]
    fn truncation_splitting_a_multibyte_char_keeps_valid_prefix() -> TestResult {
        let temp = tempfile::TempDir::new()?;
        // Pick a header length that puts the byte cap mid-character for a
        // two-byte filler char, so the cut must drop the partial char.
        let mut header = "---\ndescription: Big skill\n---\n".to_string();
        if (MAX_SKILL_BYTES - header.len()).is_multiple_of(2) {
            header.push('\n');
        }
        let fill_chars = (MAX_SKILL_BYTES - header.len()) / 2 + 16;
        let body = "é".repeat(fill_chars);
        write_skill(temp.path(), "split", &format!("{header}{body}"))?;

        let Some(skill) = load_skill(temp.path(), "split", MAX_SKILL_BYTES) else {
            panic!("expected truncated multibyte skill to load");
        };
        assert!(skill.body.len() < MAX_SKILL_BYTES);
        assert!(skill.body.chars().all(|c| c == 'é'));
        Ok(())
    }

    #[test]
    fn invalid_utf8_skill_md_is_rejected() -> TestResult {
        let temp = tempfile::TempDir::new()?;
        let dir = skills_dir(temp.path()).join("binary");
        std::fs::create_dir_all(&dir)?;
        let mut bytes = b"---\ndescription: Binary\n---\n".to_vec();
        bytes.extend_from_slice(&[0xff, 0xfe, 0x00]);
        std::fs::write(dir.join("SKILL.md"), bytes)?;

        assert!(load_skill(temp.path(), "binary", MAX_SKILL_BYTES).is_none());
        assert!(list_skills(temp.path(), MAX_SKILL_BYTES).is_empty());
        Ok(())
    }

    #[test]
    fn render_invocation_includes_body_and_args() -> TestResult {
        let temp = tempfile::TempDir::new()?;
        write_skill(
            temp.path(),
            "deploy",
            "---\ndescription: Deploy the app\n---\nRun make deploy.",
        )?;
        let Some(skill) = load_skill(temp.path(), "deploy", MAX_SKILL_BYTES) else {
            panic!("expected skill to load");
        };

        let rendered = render_skill_invocation(&skill, Some("to staging"));
        assert!(rendered.starts_with("<skill-invocation skill=\"deploy\">"));
        assert!(rendered.contains("Run make deploy."));
        assert!(rendered.ends_with("to staging"));

        let rendered = render_skill_invocation(&skill, None);
        assert!(rendered.ends_with("(no additional arguments)"));
        Ok(())
    }
}
