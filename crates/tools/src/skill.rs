use std::path::PathBuf;

use rig::{completion::ToolDefinition, tool::Tool};
use schemars::{JsonSchema, schema_for};
use serde::Deserialize;

use crate::{
    ToolError,
    skills::{list_skills, load_skill, render_skill_invocation},
};

const DESCRIPTION: &str = r#"Load a project skill and follow its instructions.

Skills are user-defined instruction packages stored in this project. Only invoke skills that appear in the "Available skills" context block; never guess a skill name. The tool returns the skill's instructions — follow them for the current request."#;

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
    cwd: PathBuf,
}

impl SkillTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self { cwd }
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
        match load_skill(&self.cwd, name) {
            Some(skill) => Ok(render_skill_invocation(&skill, args.args.as_deref())),
            None => {
                let available = list_skills(&self.cwd)
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
        let dir = crate::skills::skills_dir(root).join(name);
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

        let tool = SkillTool::new(temp.path().to_path_buf());
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

        let tool = SkillTool::new(temp.path().to_path_buf());
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
        let tool = SkillTool::new(temp.path().to_path_buf());
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
