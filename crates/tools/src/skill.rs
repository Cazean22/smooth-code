use std::path::PathBuf;

use rig::{completion::ToolDefinition, tool::Tool};
use schemars::{JsonSchema, schema_for};
use serde::Deserialize;

use crate::{
    ToolError,
    skills::{list_skills, load_skill, render_skill_invocation},
};

const DESCRIPTION: &str = r#"Load a skill and follow its instructions.

Skills are user-defined instruction packages, drawn from your user-global skills directory and from the current project; on a name clash the project's skill takes precedence. Only invoke skills that appear in the "Available skills" context block; never guess a skill name. The tool returns the skill's instructions — follow them for the current request."#;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SkillArgs {
    /// Name of the skill to load, exactly as listed in the "Available skills" context block.
    pub skill: String,
    /// Optional arguments or extra context to apply the skill with.
    #[serde(default)]
    pub args: Option<String>,
}

#[derive(Clone)]
pub struct SkillTool {
    skill_roots: Vec<PathBuf>,
    max_skill_bytes: usize,
}

impl SkillTool {
    pub fn new(skill_roots: Vec<PathBuf>) -> Self {
        Self {
            skill_roots,
            max_skill_bytes: crate::skills::MAX_SKILL_BYTES,
        }
    }

    /// Override the SKILL.md read cap (from the resolved app config).
    pub fn with_max_skill_bytes(mut self, max_skill_bytes: usize) -> Self {
        self.max_skill_bytes = max_skill_bytes;
        self
    }
}

impl Tool for SkillTool {
    const NAME: &'static str = "skill";

    type Error = ToolError;
    type Args = SkillArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: DESCRIPTION.to_string(),
            parameters: schema_for!(SkillArgs).to_value(),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let name = args.skill.trim();
        match load_skill(&self.skill_roots, name, self.max_skill_bytes) {
            Some(skill) => Ok(render_skill_invocation(&skill, args.args.as_deref())),
            None => {
                let available = list_skills(&self.skill_roots, self.max_skill_bytes)
                    .into_iter()
                    .map(|meta| meta.name)
                    .collect::<Vec<_>>();
                let available = if available.is_empty() {
                    "none".to_string()
                } else {
                    available.join(", ")
                };
                Err(ToolError::invalid_arguments(format!(
                    "unknown skill `{name}`; available skills: {available}"
                )))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn write_skill(root: &std::path::Path, name: &str, content: &str) -> TestResult {
        let dir = crate::skills::project_skills_dir(root).join(name);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("SKILL.md"), content)?;
        Ok(())
    }

    #[tokio::test]
    async fn loads_skill_body_with_args() -> TestResult {
        let temp = tempfile::TempDir::new()?;
        write_skill(
            temp.path(),
            "deploy",
            "---\ndescription: Deploy the app\n---\nRun make deploy.",
        )?;

        let tool = SkillTool::new(vec![crate::skills::project_skills_dir(temp.path())]);
        let output = tool
            .call(SkillArgs {
                skill: "deploy".to_string(),
                args: Some("to staging".to_string()),
            })
            .await?;

        assert!(output.starts_with("<skill-invocation skill=\"deploy\">"));
        assert!(output.contains("Run make deploy."));
        assert!(output.ends_with("to staging"));
        Ok(())
    }

    #[tokio::test]
    async fn unknown_skill_lists_available() -> TestResult {
        let temp = tempfile::TempDir::new()?;
        write_skill(temp.path(), "deploy", "---\ndescription: Deploy\n---\nbody")?;

        let tool = SkillTool::new(vec![crate::skills::project_skills_dir(temp.path())]);
        let result = tool
            .call(SkillArgs {
                skill: "missing".to_string(),
                args: None,
            })
            .await;

        let Err(error) = result else {
            panic!("expected unknown skill to be rejected");
        };
        assert_eq!(error.kind(), "invalid_arguments");
        assert!(error.to_string().contains("unknown skill `missing`"));
        assert!(error.to_string().contains("deploy"));
        Ok(())
    }

    #[tokio::test]
    async fn unknown_skill_with_no_skills_says_none() {
        let Ok(temp) = tempfile::TempDir::new() else {
            panic!("tempdir creation failed");
        };
        let tool = SkillTool::new(vec![crate::skills::project_skills_dir(temp.path())]);
        let result = tool
            .call(SkillArgs {
                skill: "missing".to_string(),
                args: None,
            })
            .await;

        let Err(error) = result else {
            panic!("expected unknown skill to be rejected");
        };
        assert!(error.to_string().contains("available skills: none"));
    }
}
