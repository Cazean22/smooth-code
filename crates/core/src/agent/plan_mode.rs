use std::sync::LazyLock;

/// The tools available while a thread is in plan mode, with the description
/// shown to the model for each. Single source of truth: the prompt text below
/// is generated from this list, and a provider test asserts the plan-mode
/// agent registers exactly these tools.
///
/// Mirrors Claude Code's plan-mode system-prompt: tell the model what tools
/// are allowed, what behavior is required, and what phases to follow.
pub(crate) const PLAN_MODE_TOOLS: &[(&str, &str)] = &[
    ("read", "read known files."),
    (
        "run_command",
        "run read-only shell commands for exploration and validation, such as `eza`, `fd`, \
         `rg`, and test/build checks.",
    ),
    (
        "spawn_agent",
        "spawn sub-agents for parallel read-only exploration. While in plan mode every child \
         runs as an Explore agent regardless of the `subagent_type` you pass; none of them can \
         modify files.",
    ),
    (
        "ask_user_question",
        "ask the user to clarify requirements or choose between approaches.",
    ),
    (
        "todo_write",
        "optionally track planning progress after exploration has produced concrete planning steps; \
         avoid speculative checklists.",
    ),
    (
        "skill",
        "load a skill's instructions by name; only invoke skills listed in the \
         Available Skills context block.",
    ),
    ("plan_write", "write your plan to the per-thread plan file."),
    (
        "exit_plan_mode",
        "submit the plan you wrote with `plan_write` for user approval or continued discussion.",
    ),
];

pub(crate) fn plan_mode_tool_names() -> impl Iterator<Item = &'static str> {
    PLAN_MODE_TOOLS.iter().map(|(name, _)| *name)
}

/// Instructions appended to the agent preamble when a thread is in plan mode.
pub(crate) fn plan_mode_instructions() -> &'static str {
    static INSTRUCTIONS: LazyLock<String> = LazyLock::new(|| {
        let tool_list = PLAN_MODE_TOOLS
            .iter()
            .map(|(name, description)| format!("- `{name}` — {description}"))
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "You are in PLAN MODE.

While in plan mode you may only use these tools:
{tool_list}

You MUST NOT edit files or write to arbitrary paths while in plan mode. Use `run_command` only for read-only inspection or validation commands; do not run shell commands that modify files or system state. \
Delete, edit, and write tools are unavailable. Never claim to have changed code while in plan mode.

Proceed in four phases. During EXPLORE, prefer progress updates and read-only tools; use `todo_write` only after the planning work is concrete enough that the checklist communicates useful status:
1. EXPLORE — read the relevant code and gather context for the user's request.
2. DESIGN — decide on the approach, considering trade-offs and conventions you observed.
3. WRITE — call `plan_write` with a markdown plan covering: goal, files to change, step-by-step strategy, risks, and any decisions needing user confirmation.
4. SUBMIT — call `exit_plan_mode` to present the plan to the user for approval. If they approve, plan mode turns off and you implement the plan with the full tool set. If they reject, you stay in plan mode: revise the plan per their feedback with `plan_write`, then submit it again. If they choose to continue chatting, stay in plan mode, answer their message, and yield back to the user; do not call `plan_write` or `exit_plan_mode` again unless the user asks or provides new direction.
"
        )
    });
    &INSTRUCTIONS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instructions_list_every_plan_mode_tool() {
        let instructions = plan_mode_instructions();
        for name in plan_mode_tool_names() {
            assert!(
                instructions.contains(&format!("- `{name}` — ")),
                "plan-mode instructions should list `{name}`"
            );
        }
    }
}
