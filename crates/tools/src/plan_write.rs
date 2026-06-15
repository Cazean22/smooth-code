use std::path::{Path, PathBuf};

use cazean_protocol::ThreadId;
use rig::{completion::ToolDefinition, tool::Tool};
use schemars::{JsonSchema, schema_for};
use serde::Deserialize;

use crate::ToolError;

const DESCRIPTION: &str = r#"Write or overwrite this thread's plan file.

Usage:
- Call this tool while in plan mode to record the agreed-upon plan as markdown.
- The file is always written to `<workspace>/.cazean/plans/<thread_id>.md`; you do not choose the path.
- The previous contents (if any) are replaced. Call this tool again to refine the plan.
- After the plan is ready, call `exit_plan_mode` to submit it for user approval."#;

/// Absolute path of the per-thread plan file under the workspace root. Single
/// source of the plan-file location for both `plan_write` and the core
/// `exit_plan_mode` approval flow.
pub fn plan_file_path(workspace_root: &Path, thread_id: ThreadId) -> PathBuf {
    workspace_root
        .join(".cazean")
        .join("plans")
        .join(format!("{thread_id}.md"))
}

#[derive(Clone)]
pub struct PlanWriteTool {
    workspace_root: PathBuf,
    thread_id: ThreadId,
}

impl PlanWriteTool {
    pub fn new(workspace_root: PathBuf, thread_id: ThreadId) -> Self {
        Self {
            workspace_root,
            thread_id,
        }
    }

    fn plan_path(&self) -> PathBuf {
        plan_file_path(&self.workspace_root, self.thread_id)
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlanWriteArgs {
    /// Markdown body of the plan. Replaces any previous plan for this thread.
    content: String,
}

impl Tool for PlanWriteTool {
    const NAME: &'static str = "plan_write";

    type Error = ToolError;
    type Args = PlanWriteArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: DESCRIPTION.to_string(),
            parameters: schema_for!(PlanWriteArgs).to_value(),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let path = self.plan_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|err| {
                ToolError::io(format!(
                    "failed to create plans directory {}: {err}",
                    parent.display()
                ))
            })?;
        }
        std::fs::write(&path, &args.content).map_err(|err| {
            ToolError::io(format!("failed to write plan {}: {err}", path.display()))
        })?;
        Ok(format!("wrote plan to {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use cazean_protocol::ThreadId;
    use tempfile::TempDir;

    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[tokio::test]
    async fn writes_plan_file_under_cazean_plans() -> TestResult {
        let tmp = TempDir::new()?;
        let thread_id = ThreadId::new();
        let tool = PlanWriteTool::new(tmp.path().to_path_buf(), thread_id);

        let output = tool
            .call(PlanWriteArgs {
                content: "# Plan\n\nstep 1".to_string(),
            })
            .await?;

        let path = tmp
            .path()
            .join(".cazean")
            .join("plans")
            .join(format!("{thread_id}.md"));
        assert!(output.contains(&path.display().to_string()));
        let written = fs::read_to_string(&path)?;
        assert_eq!(written, "# Plan\n\nstep 1");
        Ok(())
    }

    #[tokio::test]
    async fn second_call_overwrites_previous_plan() -> TestResult {
        let tmp = TempDir::new()?;
        let thread_id = ThreadId::new();
        let tool = PlanWriteTool::new(tmp.path().to_path_buf(), thread_id);

        tool.call(PlanWriteArgs {
            content: "first".to_string(),
        })
        .await?;
        tool.call(PlanWriteArgs {
            content: "second".to_string(),
        })
        .await?;

        let path = tmp
            .path()
            .join(".cazean")
            .join("plans")
            .join(format!("{thread_id}.md"));
        let written = fs::read_to_string(&path)?;
        assert_eq!(written, "second");
        Ok(())
    }
}
