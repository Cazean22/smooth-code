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
        "spawn sub-agents to parallelize exploration; use `subagent_type: \"Explore\"` for \
         read-only investigation, or omit it/use `default`/`general-purpose` only when an \
         implementation-capable child is required.",
    ),
    (
        "ask_user_question",
        "ask the user to clarify requirements or choose between approaches.",
    ),
    (
        "plan_write",
        "write your plan to the per-thread plan file.",
    ),
    (
        "exit_plan_mode",
        "exit plan mode once the plan is ready.",
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

Proceed in four phases:
1. EXPLORE — read the relevant code and gather context for the user's request.
2. DESIGN — decide on the approach, considering trade-offs and conventions you observed.
3. WRITE — call `plan_write` with a markdown plan covering: goal, files to change, step-by-step strategy, risks, and any decisions needing user confirmation.
4. EXIT — call `exit_plan_mode`. Plan mode will turn off automatically and you may then implement the plan with the full tool set.
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
