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
Todoist-first and centered on Symphony's structured `todoist` tool rather than
raw HTTP calls.

## Primary tool

Use the `todoist` dynamic tool exposed by Symphony's app-server session. It
reuses the session's configured Todoist auth and runtime guardrails.

Tool input:

```json
{
  "action": "todoist action name",
  "other_fields": "action-specific arguments"
}
```

Tool behavior:

- Send one Todoist action per tool call.
- Treat structured error payloads as real failures even if the tool call itself
  completed.
- Keep requests narrowly scoped and ask only for the fields or objects you
  need.
- Preserve Todoist ids as opaque strings.
- Remember that many list endpoints are cursor-paginated.

## Canonical sources

When you need to confirm current Todoist behavior, prefer these sources in this
order:

1. Symphony's local Todoist action contract in
   `../../../rust-todoist/src/dynamic_tool.rs`
2. Runtime guidance in `../../../rust-todoist/README.md` and
   `../../../rust-todoist/WORKFLOW.md`
3. Official Todoist API v1 docs at `https://developer.todoist.com/api/v1/`

Important Todoist API status:

- Todoist API v1 is the current official API.
- REST v2 docs are deprecated and should be treated as migration context, not
  the primary source of truth.
- Comments, reminders, and activity logs are plan-dependent capabilities. The
  runtime validates these where required, but you should still expect capability
  errors when a feature is unavailable.

## Discovering unfamiliar operations

There is no GraphQL-style introspection for the `todoist` tool. When you need
an unfamiliar action or argument shape:

- Inspect `todoist_action_contract` in
  `../../../rust-todoist/src/dynamic_tool.rs`
- Check `../../../rust-todoist/README.md` for runtime-specific semantics
- Use the official Todoist API v1 docs to confirm upstream behavior such as
  pagination, ids, close semantics, or move semantics

Start with these targeted reads rather than exploratory calls:

- `get_current_user`
- `get_task`
- `list_sections`
- `list_tasks`
- `list_comments`
- `get_workpad`
- `list_activities`

## Core Symphony semantics

Treat these as part of the tool contract, not optional style:

- Non-workpad task comments are human review input. Read them and respond to
  them in your work.
- The workpad is the only agent-owned task comment. Use `get_workpad`,
  `upsert_workpad`, and `delete_workpad` for it.
- Do not overwrite, edit, or delete human task comments.
- Do not use generic task comment creation for Symphony notes. The runtime
  rejects that path on purpose.
- `list_comments` requires exactly one of `task_id` or `project_id`.
- `close_task` is workflow-guarded. In the Rust Todoist runtime it only succeeds
  when the workflow/state machine allows it, typically from `Merging` after the
  linked PR is confirmed merged.
- `create_task` defaults into the project's `Todo` section when no
  `section_id` is provided. Use `parent_id` only for true subtasks.
- When `tracker.label` is configured for the runtime, `create_task` inherits it
  automatically.

## Common workflows

### Resolve the current user and project context

Read the current Todoist user:

```json
{
  "action": "get_current_user"
}
```

List projects:

```json
{
  "action": "list_projects",
  "limit": 50
}
```

List labels:

```json
{
  "action": "list_labels",
  "limit": 50
}
```

### Query a task by id

Use `get_task` once you know the Todoist task id:

```json
{
  "action": "get_task",
  "task_id": "6X7gfV9G7rWm5hW8"
}
```

### Inspect sections before moving or creating tasks

List sections for a project:

```json
{
  "action": "list_sections",
  "project_id": "6Jf8VQXxpwv56VQ7",
  "limit": 100
}
```

Read one section directly when you already have its id:

```json
{
  "action": "get_section",
  "section_id": "6X7gfh25J2x4p7R4"
}
```

### List tasks in a project, section, subtree, or filter

List active tasks in a project:

```json
{
  "action": "list_tasks",
  "project_id": "6Jf8VQXxpwv56VQ7",
  "limit": 100
}
```

List tasks in a section:

```json
{
  "action": "list_tasks",
  "section_id": "6X7gfh25J2x4p7R4",
  "limit": 100
}
```

List subtasks under a parent:

```json
{
  "action": "list_tasks",
  "parent_id": "6X7gfV9G7rWm5hW8",
  "limit": 100
}
```

List tasks by filter query:

```json
{
  "action": "list_tasks",
  "filter": "today & #Engineering",
  "limit": 100
}
```

### Read human comments on a task

Read task comments before rework, review response, or merge work:

```json
{
  "action": "list_comments",
  "task_id": "6X7gfV9G7rWm5hW8",
  "limit": 100
}
```

Interpretation rules:

- Treat non-workpad task comments as human instructions.
- Ignore the single `## Codex Workpad` comment as review input.
- If comments are paginated, keep following `cursor` until you have enough
  context.

### Read and update the workpad

Fetch the canonical task workpad:

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

Delete it when the workflow explicitly calls for cleanup:

```json
{
  "action": "delete_workpad",
  "task_id": "6X7gfV9G7rWm5hW8"
}
```

### Create and manage project comments

Use project comments for project-scoped notes, not task workpads.

Create a project comment:

```json
{
  "action": "create_project_comment",
  "project_id": "6Jf8VQXxpwv56VQ7",
  "content": "Release train is blocked on production validation."
}
```

Read a specific comment:

```json
{
  "action": "get_comment",
  "comment_id": "6X7gfQHG59V8CJJV"
}
```

Update a project comment:

```json
{
  "action": "update_comment",
  "comment_id": "6X7gfQHG59V8CJJV",
  "content": "Release train is unblocked after production validation."
}
```

Delete a project comment:

```json
{
  "action": "delete_comment",
  "comment_id": "6X7gfQHG59V8CJJV"
}
```

Do not use `update_comment` or `delete_comment` on task-scoped human comments.
For task-scoped Symphony notes, use the workpad helpers instead.

### Update a task

Use `update_task` for field edits that keep the task in place:

```json
{
  "action": "update_task",
  "task_id": "6X7gfV9G7rWm5hW8",
  "content": "Ship Todoist merge preflight hardening",
  "description": "Refresh Todoist comments in Merging and Rework.",
  "priority": 4,
  "labels": [
    "symphony"
  ]
}
```

### Move a task to another project, section, or parent

Use `move_task` when the task's location should change. Pass only the
destination you intend.

Move to a different section:

```json
{
  "action": "move_task",
  "task_id": "6X7gfV9G7rWm5hW8",
  "section_id": "6X7gh3g5v7v7W8hG"
}
```

Move to another project:

```json
{
  "action": "move_task",
  "task_id": "6X7gfV9G7rWm5hW8",
  "project_id": "6Jf8VQXxpwv56VQ7"
}
```

Convert it into a subtask:

```json
{
  "action": "move_task",
  "task_id": "6X7gfV9G7rWm5hW8",
  "parent_id": "6X7gh7F7H5w3fWQ9"
}
```

### Create a task or follow-up

Create a top-level task:

```json
{
  "action": "create_task",
  "content": "Investigate merge gate drift",
  "description": "Audit merge-time handling of review comments and Todoist notes.",
  "project_id": "6Jf8VQXxpwv56VQ7",
  "priority": 3
}
```

Create a task directly in a section:

```json
{
  "action": "create_task",
  "content": "Follow up on unresolved review threads",
  "project_id": "6Jf8VQXxpwv56VQ7",
  "section_id": "6X7gh3g5v7v7W8hG"
}
```

Create a subtask:

```json
{
  "action": "create_task",
  "content": "Resolve Copilot thread state handling",
  "parent_id": "6X7gh7F7H5w3fWQ9",
  "origin_task_id": "6X7gfV9G7rWm5hW8"
}
```

### Close or reopen a task

Close a task only when the workflow permits completion:

```json
{
  "action": "close_task",
  "task_id": "6X7gfV9G7rWm5hW8"
}
```

Reopen a completed task:

```json
{
  "action": "reopen_task",
  "task_id": "6X7gfV9G7rWm5hW8"
}
```

Remember that Todoist close semantics are client-like:

- Regular tasks move to history
- Recurring tasks advance to the next occurrence

### Inspect activity history

Use activities when you need audit-style context about who changed what and
when.

List recent activity for a task:

```json
{
  "action": "list_activities",
  "object_type": "item",
  "object_id": "6X7gfV9G7rWm5hW8",
  "annotate_notes": true,
  "annotate_parents": true,
  "limit": 50
}
```

List recent activity for a project:

```json
{
  "action": "list_activities",
  "parent_project_id": "6Jf8VQXxpwv56VQ7",
  "annotate_notes": true,
  "limit": 50
}
```

Activity logs are cursor-paginated and plan-dependent.

### Manage reminders

List reminders:

```json
{
  "action": "list_reminders",
  "task_id": "6X7gfV9G7rWm5hW8",
  "limit": 50
}
```

Create a reminder:

```json
{
  "action": "create_reminder",
  "task_id": "6X7gfV9G7rWm5hW8",
  "type": "relative",
  "minute_offset": 30
}
```

Update a reminder:

```json
{
  "action": "update_reminder",
  "reminder_id": "6X7gh2Q24Qq4F5R9",
  "minute_offset": 15
}
```

Delete a reminder:

```json
{
  "action": "delete_reminder",
  "reminder_id": "6X7gh2Q24Qq4F5R9"
}
```

## Practical guidance

- Before doing review response or merge work, refresh both GitHub feedback and
  Todoist task comments.
- In `Rework`, treat actionable Todoist comments the same way you would treat
  actionable human GitHub comments.
- In `Merging`, treat Todoist comments as merge-relevant human input even though
  the final close operation remains workflow-guarded.
- Prefer `list_sections` plus exact section-name matching over guessing section
  ids.
- Prefer `get_task` before `update_task` or `move_task` when you are not certain
  about the task's current project, section, or parent.
- Follow pagination when the first page may be incomplete.
- If an action fails due to capability or validation constraints, adjust to the
  runtime-supported path instead of attempting raw API workarounds.
