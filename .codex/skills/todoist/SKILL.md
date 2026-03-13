---
name: todoist
description: |
  Use Symphony's `todoist` dynamic tool for Todoist task, comment, workpad,
  reminder, activity-log, and state-transition operations during Rust Todoist
  runtime sessions.
---

# Todoist Dynamic Tool

Use this skill for Todoist-backed Symphony work during Rust app-server sessions.

This is the Todoist counterpart to the repo-local `linear` skill, but it is
centered on Symphony's structured `todoist` tool and the Rust Todoist runtime's
workflow guardrails.

## Primary tool

Use the `todoist` dynamic tool exposed by Symphony's app-server session.

Tool input:

```json
{
  "action": "todoist action name",
  "other_fields": "action-specific arguments"
}
```

Tool behavior:

- Send one Todoist action per tool call.
- Treat structured error payloads as real failures even if the tool call
  itself completed.
- Keep requests narrowly scoped.
- Preserve Todoist ids as opaque strings.
- Expect cursor pagination on many list actions.

## Canonical sources

Prefer these sources in order:

1. `../../../rust-todoist/src/dynamic_tool.rs`
2. `../../../rust-todoist/README.md`
3. `../../../rust-todoist/WORKFLOW.md`
4. Official Todoist API v1 docs: `https://developer.todoist.com/api/v1/`

Use deprecated REST v2 docs only as migration context.

When you need more examples than this file includes, read
`references/action-recipes.md`.

## Discovering unfamiliar operations

There is no GraphQL-style introspection for `todoist`. When you need an action
shape:

- inspect `todoist_action_contract` in
  `../../../rust-todoist/src/dynamic_tool.rs`
- confirm runtime-specific semantics in the Rust Todoist README and workflow
- use the official Todoist API v1 docs only to confirm upstream behavior such
  as pagination, ids, move semantics, and close semantics

Start with these narrow reads rather than exploratory calls:

- `get_task`
- `list_sections`
- `list_tasks`
- `list_comments`
- `get_workpad`
- `list_activities`

## Core runtime rules

Treat these as hard constraints, not style:

- Non-workpad task comments are human review input.
- The workpad is the only agent-owned task comment. Use `get_workpad`,
  `upsert_workpad`, and `delete_workpad` for it.
- Do not edit or delete human task comments.
- Do not use generic task comment creation for Symphony notes. The runtime
  rejects that path on purpose.
- `list_comments` requires exactly one of `task_id` or `project_id`.
- `close_task` is workflow-guarded and normally only succeeds from `Merging`
  after the linked PR is confirmed merged.
- `create_task` defaults into the project's `Todo` section when no
  `section_id` is provided.
- Use `parent_id` only for true subtasks.
- If `tracker.label` is configured, `create_task` inherits it automatically.
- Comments, reminders, and activity logs are plan-dependent capabilities.

## High-value workflows

### Read task context

Start with the task itself:

```json
{
  "action": "get_task",
  "task_id": "6X7gfV9G7rWm5hW8"
}
```

If you need placement context, list sections or tasks:

```json
{
  "action": "list_sections",
  "project_id": "6Jf8VQXxpwv56VQ7",
  "limit": 100
}
```

```json
{
  "action": "list_tasks",
  "project_id": "6Jf8VQXxpwv56VQ7",
  "limit": 100
}
```

### Read human review input

Refresh task comments before rework, review response, or merge work:

```json
{
  "action": "list_comments",
  "task_id": "6X7gfV9G7rWm5hW8",
  "limit": 100
}
```

Interpretation rules:

- treat non-workpad task comments as human instructions
- ignore the single `## Codex Workpad` comment as review input
- follow `cursor` when the first page is incomplete

### Read or update the workpad

Fetch the canonical workpad:

```json
{
  "action": "get_workpad",
  "task_id": "6X7gfV9G7rWm5hW8"
}
```

Create or update it:

```json
{
  "action": "upsert_workpad",
  "task_id": "6X7gfV9G7rWm5hW8",
  "content": "## Codex Workpad\n- Investigating failing smoke check\n- Next: update PR after rerun"
}
```

### Update task state or content

Use `update_task` for in-place edits:

```json
{
  "action": "update_task",
  "task_id": "6X7gfV9G7rWm5hW8",
  "content": "Ship Todoist merge preflight hardening",
  "description": "Refresh Todoist comments in Merging and Rework."
}
```

Use `move_task` when location changes. Pass only the destination you intend:

```json
{
  "action": "move_task",
  "task_id": "6X7gfV9G7rWm5hW8",
  "section_id": "6X7gh3g5v7v7W8hG"
}
```

### Create follow-up work

Create a top-level task:

```json
{
  "action": "create_task",
  "content": "Investigate merge gate drift",
  "project_id": "6Jf8VQXxpwv56VQ7"
}
```

Create a subtask only when the new work is truly nested:

```json
{
  "action": "create_task",
  "content": "Resolve Copilot thread state handling",
  "parent_id": "6X7gh7F7H5w3fWQ9",
  "origin_task_id": "6X7gfV9G7rWm5hW8"
}
```

### Complete or reopen a task

Close a task only when the workflow permits completion:

```json
{
  "action": "close_task",
  "task_id": "6X7gfV9G7rWm5hW8"
}
```

Reopen when needed:

```json
{
  "action": "reopen_task",
  "task_id": "6X7gfV9G7rWm5hW8"
}
```

## Workflow alignment

- Before review response or merge work, refresh both GitHub feedback and
  Todoist task comments.
- In `Rework`, treat actionable Todoist comments the same way as actionable
  human GitHub feedback.
- In `Merging`, treat Todoist comments as merge-relevant human input even
  though bot feedback remains advisory by default.
- Prefer `list_sections` plus exact section-name matching over guessing ids.
- Prefer `get_task` before `update_task` or `move_task` when current placement
  is uncertain.
- If an action fails due to capability or validation constraints, adjust to the
  runtime-supported path instead of attempting raw API workarounds.

## Reference map

Load `references/action-recipes.md` when you need:

- project comment actions
- activity log recipes
- reminder actions
- expanded task move and create examples
- a fuller action catalog than this core skill includes
