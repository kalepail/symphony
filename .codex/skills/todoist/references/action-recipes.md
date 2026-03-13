# Todoist Action Recipes

Use this file only when the core skill is not enough.

## Contents

- Resolve current user and broad project context
- Query sections, tasks, and filters
- Project comment actions
- Expanded task mutation examples
- Activity log recipes
- Reminder recipes

## Resolve current user and broad project context

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

## Query sections, tasks, and filters

Read one section directly:

```json
{
  "action": "get_section",
  "section_id": "6X7gfh25J2x4p7R4"
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

List tasks by filter:

```json
{
  "action": "list_tasks",
  "filter": "today & #Engineering",
  "limit": 100
}
```

## Project comment actions

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

## Expanded task mutation examples

Move to another project:

```json
{
  "action": "move_task",
  "task_id": "6X7gfV9G7rWm5hW8",
  "project_id": "6Jf8VQXxpwv56VQ7"
}
```

Convert a task into a subtask:

```json
{
  "action": "move_task",
  "task_id": "6X7gfV9G7rWm5hW8",
  "parent_id": "6X7gh7F7H5w3fWQ9"
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

Create a richer top-level task:

```json
{
  "action": "create_task",
  "content": "Investigate merge gate drift",
  "description": "Audit merge-time handling of review comments and Todoist notes.",
  "project_id": "6Jf8VQXxpwv56VQ7",
  "priority": 3
}
```

Update a task with labels and priority:

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

## Activity log recipes

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

## Reminder recipes

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
