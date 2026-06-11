---
description: Inspect, validate, and commit current changes with a concise summary plus detailed commit body
---
# commit-changes skill

Use this skill when the user asks to commit the current changes, prepare a git commit, or generate and apply a commit message.

## Goal

Create a safe, accurate git commit for the requested changes. The commit message must include a concise summary line and a useful detailed body explaining what changed and why.

## Commit process

1. Inspect the working tree:
   - Run `git status --short`.
   - Review staged and unstaged changes with `git diff --staged` and `git diff` as needed.
   - If there are untracked files, inspect enough of them to decide whether they belong in the commit.
2. Determine the commit scope:
   - If the user specified files or a subset of changes, commit only that scope.
   - Otherwise commit all relevant current changes, but do not include unrelated local edits, generated artifacts, secrets, logs, or temporary files.
3. Validate when practical:
   - Prefer narrow tests or checks related to the changed area.
   - If validation is skipped or fails because of pre-existing/unrelated issues, mention that clearly before committing when it affects confidence.
4. Stage the intended files explicitly.
5. Create the commit with a multi-line message:
   - Summary line: imperative mood, concise, ideally 50 characters or fewer and no trailing period.
   - Blank line.
   - Body: include concrete details beyond the summary, typically bullets covering what changed, why it changed, and relevant validation or caveats.
6. After committing, report the new commit hash, summary, files included, and validation performed.

## Message format

Use this structure unless the user requests another format:

```text
<imperative summary>

- <detail about the main behavior or code change>
- <detail about tests, validation, docs, migrations, or compatibility>
- <detail about caveats, follow-up, or intentionally excluded work when relevant>
```

## Safety rules

- Do not amend, rebase, reset, stash, or push unless the user explicitly asks.
- Do not overwrite or discard user changes.
- Do not include secrets or environment-specific files.
- If the intended commit scope is ambiguous or risky, ask one concise clarification question before staging or committing.
- If there is nothing to commit, say so and do not create an empty commit unless explicitly requested.
