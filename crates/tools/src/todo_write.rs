use cazean_protocol::{TodoItem, TodoStatus};
use rig::{completion::ToolDefinition, tool::Tool};
use schemars::{JsonSchema, schema_for};
use serde::Deserialize;

use crate::{ToolError, output::encode_tool_output_with_todos};

const DESCRIPTION: &str = r#"Track verified progress on substantial multi-step work with a session-scoped todo checklist that the user sees live in the UI.

When to use:
- After initial investigation has identified concrete, task-specific steps and the work has 3 or more distinct steps.
- The user gives you several tasks at once, or asks you to track progress.
- Right after a plan is approved: capture the implementation steps before starting.

When NOT to use:
- Before inspecting the relevant context, as a reflexive first action, or for generic lists like "inspect, implement, test".
- When the next actions are still uncertain; keep gathering context until a checklist would be grounded and useful.
- Single, straightforward tasks; trivial work where tracking adds no value.
- Purely conversational or informational requests.

Usage:
- Each call REPLACES the entire list. Always send the complete updated list, never a delta.
- Each todo has `content` (short imperative phrase) and `status`: pending | in_progress | completed.
- Make each todo specific to the discovered task, naming concrete files, components, or outcomes when known.
- Keep the list minimal; prefer a brief progress update over a checklist that does not help the user understand real work.
- Mark a todo in_progress BEFORE starting that step. Keep EXACTLY ONE todo in_progress at a time.
- Mark a todo completed IMMEDIATELY after finishing it; do not batch completions.
- Only mark completed when the step fully succeeded. If you hit errors or blockers, keep it in_progress and add a new todo describing what must be resolved.
- Add newly discovered steps as you work; drop todos that became irrelevant.
- Send an empty list to clear the checklist."#;

const MAX_TODOS: usize = 50;

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TodoWriteArgs {
    /// The complete todo list. Each call REPLACES the previous list entirely.
    pub todos: Vec<TodoInput>,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TodoInput {
    /// Short imperative description of the step (e.g. "Add registration test").
    pub content: String,
    /// pending | in_progress | completed
    pub status: TodoStatus,
}

#[derive(Clone)]
pub struct TodoWriteTool {
    max_todos: usize,
}

impl TodoWriteTool {
    pub fn new() -> Self {
        Self {
            max_todos: MAX_TODOS,
        }
    }

    /// Override the maximum todo count (from the resolved app config).
    pub fn with_max_todos(mut self, max_todos: usize) -> Self {
        self.max_todos = max_todos;
        self
    }
}

impl Default for TodoWriteTool {
    fn default() -> Self {
        Self::new()
    }
}

fn validate_args(args: &TodoWriteArgs, max_todos: usize) -> Result<(), ToolError> {
    if args.todos.len() > max_todos {
        return Err(ToolError::invalid_arguments(format!(
            "todo list has {} items; the maximum is {max_todos}",
            args.todos.len()
        )));
    }
    if args.todos.iter().any(|todo| todo.content.trim().is_empty()) {
        return Err(ToolError::invalid_arguments(
            "todo content must not be empty",
        ));
    }
    let in_progress = args
        .todos
        .iter()
        .filter(|todo| todo.status == TodoStatus::InProgress)
        .count();
    if in_progress > 1 {
        return Err(ToolError::invalid_arguments(format!(
            "{in_progress} todos are in_progress; keep exactly one in progress at a time"
        )));
    }
    Ok(())
}

fn summarize(todos: &[TodoItem]) -> String {
    let completed = todos
        .iter()
        .filter(|todo| todo.status == TodoStatus::Completed)
        .count();
    let in_progress = todos
        .iter()
        .filter(|todo| todo.status == TodoStatus::InProgress)
        .count();
    let pending = todos.len() - completed - in_progress;
    format!(
        "Todo list updated: {} items ({completed} completed, {in_progress} in progress, {pending} pending).",
        todos.len()
    )
}

impl Tool for TodoWriteTool {
    const NAME: &'static str = "todo_write";

    type Error = ToolError;
    type Args = TodoWriteArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: Self::NAME.to_string(),
            description: DESCRIPTION.to_string(),
            parameters: schema_for!(TodoWriteArgs).to_value(),
        }
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        validate_args(&args, self.max_todos)?;
        if args.todos.is_empty() {
            return Ok("Todo list cleared.".to_string());
        }
        let todos = args
            .todos
            .into_iter()
            .map(|todo| TodoItem {
                content: todo.content,
                status: todo.status,
            })
            .collect::<Vec<_>>();
        Ok(encode_tool_output_with_todos(summarize(&todos), todos))
    }
}

#[cfg(test)]
mod tests {
    use crate::output::decode_tool_output_for_tool;

    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn todo(content: &str, status: TodoStatus) -> TodoInput {
        TodoInput {
            content: content.to_string(),
            status,
        }
    }

    #[tokio::test]
    async fn definition_discourages_premature_generic_lists() -> TestResult {
        let tool = TodoWriteTool::new();
        let definition = tool.definition(String::new()).await;

        assert!(
            definition
                .description
                .contains("After initial investigation has identified concrete"),
            "todo_write should be grounded in prior context"
        );
        assert!(
            definition
                .description
                .contains("as a reflexive first action"),
            "todo_write should discourage reflexive checklist creation"
        );
        assert!(
            definition
                .description
                .contains("generic lists like \"inspect, implement, test\""),
            "todo_write should discourage vague generic lists"
        );
        Ok(())
    }

    #[tokio::test]
    async fn replaces_list_and_encodes_todos() -> TestResult {
        let tool = TodoWriteTool::new();
        let output = tool
            .call(TodoWriteArgs {
                todos: vec![
                    todo("add module", TodoStatus::Completed),
                    todo("register tool", TodoStatus::InProgress),
                    todo("update tui", TodoStatus::Pending),
                ],
            })
            .await?;

        let decoded = decode_tool_output_for_tool("todo_write", output, true);
        assert_eq!(
            decoded.model_output,
            "Todo list updated: 3 items (1 completed, 1 in progress, 1 pending)."
        );
        assert_eq!(decoded.todos.len(), 3);
        assert_eq!(decoded.todos[1].content, "register tool");
        assert_eq!(decoded.todos[1].status, TodoStatus::InProgress);
        assert!(decoded.file_changes.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn empty_list_clears_without_structured_payload() -> TestResult {
        let tool = TodoWriteTool::new();
        let output = tool.call(TodoWriteArgs { todos: Vec::new() }).await?;

        assert_eq!(output, "Todo list cleared.");
        let decoded = decode_tool_output_for_tool("todo_write", output, true);
        assert!(decoded.todos.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn rejects_blank_content() {
        let tool = TodoWriteTool::new();
        let result = tool
            .call(TodoWriteArgs {
                todos: vec![todo("   ", TodoStatus::Pending)],
            })
            .await;

        let Err(error) = result else {
            panic!("expected blank content to be rejected");
        };
        assert_eq!(error.kind(), "invalid_arguments");
    }

    #[tokio::test]
    async fn rejects_multiple_in_progress() {
        let tool = TodoWriteTool::new();
        let result = tool
            .call(TodoWriteArgs {
                todos: vec![
                    todo("one", TodoStatus::InProgress),
                    todo("two", TodoStatus::InProgress),
                ],
            })
            .await;

        let Err(error) = result else {
            panic!("expected multiple in_progress todos to be rejected");
        };
        assert_eq!(error.kind(), "invalid_arguments");
    }

    #[tokio::test]
    async fn rejects_oversized_list() {
        let tool = TodoWriteTool::new();
        let result = tool
            .call(TodoWriteArgs {
                todos: (0..=MAX_TODOS)
                    .map(|i| todo(&format!("step {i}"), TodoStatus::Pending))
                    .collect(),
            })
            .await;

        let Err(error) = result else {
            panic!("expected oversized list to be rejected");
        };
        assert_eq!(error.kind(), "invalid_arguments");
    }
}
